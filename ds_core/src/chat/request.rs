//! 对话分流模块 —— prompt 超限判断与三种请求路径
//!
//! - 正常路径（v0_chat_once）：完整 prompt 直发
//! - 历史拆分路径（v0_chat_oversized_file）：超限 default 模型，拆历史为文件上传
//! - 分块路径（v0_chat_oversized_chunk）：超限 expert 模型，分块 completion 写入

use std::pin::Pin;
use std::time::Instant;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::CoreError;
use crate::accounts::CompletionPayload;

use super::response::{
    ActiveSession, ResponseStream, SessionHandle, StreamEvent, check_hint, parse_json_error,
    parse_ready_message_ids, split_two_events, wait_close, wait_ready_and_update,
};

// ── 常量 ──────────────────────────────────────────────────────────────

const TAG_START: &str = "<｜";
const TAG_END: &str = "｜>";
const SESSION_HISTORY_FILE: &str = "EMPTY.txt";

// ── Session 泄漏防护 ──────────────────────────────────────────────────

/// 临时 session 的 RAII 守卫
///
/// 每个 session 都占用服务端资源，「创建后不删除」会留下大量孤儿会话
/// （既是资源泄漏，也是风控可见的异常行为）。
///
/// 早期实现在每条错误路径上手写 `delete_session`，但 `?` 传播会直接跳过，
/// 例如分块路径的 `wait_ready_and_update(..).await?` 与 `wait_close(..).await?`。
/// 因此改为守卫式：拿到 session 后删除责任即交给守卫，
/// 正常路径用 [`SessionGuard::disarm`] 把责任移交给 `SessionHandle`。
struct SessionGuard {
    client: crate::accounts::DsClient,
    token: String,
    session_id: String,
    armed: bool,
}

impl SessionGuard {
    fn new(client: crate::accounts::DsClient, token: &str, session_id: &str) -> Self {
        Self {
            client,
            token: token.to_string(),
            session_id: session_id.to_string(),
            armed: true,
        }
    }

    /// 放弃清理责任（session 已交由 `SessionHandle` 在流结束时删除）
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Drop 不能 await；与 SessionHandle::cleanup 一致，spawn 到运行时
        let client = self.client.clone();
        let token = self.token.clone();
        let session_id = self.session_id.clone();
        log::debug!(
            target: "ds_core::accounts",
            "session guard deleting orphan session: id={}", session_id
        );
        tokio::spawn(async move {
            if let Err(e) = client.delete_session(&token, &session_id).await {
                log::warn!(
                    target: "ds_core::accounts",
                    "delete_session failed for {}: {}", session_id, e
                );
            }
        });
    }
}

// ── 公开类型 ──────────────────────────────────────────────────────────

/// 文件载荷
#[derive(Debug, Clone)]
pub struct FilePayload {
    pub filename: String,
    pub content: Vec<u8>,
    pub content_type: String,
}

/// 对话请求
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub prompt: String,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
    pub model_type: String,
    pub files: Vec<FilePayload>,
}

/// v0_chat 返回值：精简协议事件流
pub struct ChatResponse {
    pub stream: Pin<Box<dyn Stream<Item = Result<StreamEvent, CoreError>> + Send>>,
}

// ── Chat 请求方法 ─────────────────────────────────────────────────────

use super::Chat;

impl Chat {
    /// 对话入口：判断 prompt 大小，选择正常路径或回退方案
    pub async fn v0_chat(
        &self,
        req: ChatRequest,
        request_id: &str,
    ) -> Result<ChatResponse, CoreError> {
        let limit = self.input_character_limit_for(&req.model_type);
        let threshold = (limit as u64 * 75 / 100) as usize;
        let oversized = req.prompt.chars().count() > threshold;

        // 超限时按模型类型选择回退方案
        if oversized {
            log::debug!(
                target: "ds_core::accounts",
                "req={} prompt 超限 ({} chars > {} threshold), model_type={}, 触发回退方案",
                request_id,
                req.prompt.chars().count(),
                threshold,
                req.model_type,
            );
            return match req.model_type.as_str() {
                "expert" => self.v0_chat_oversized_chunk(&req, request_id).await,
                _ => self.v0_chat_oversized_file(&req, request_id).await,
            };
        }

        // 不超限：所有模型统一直发（完整 prompt，无历史拆分，无文件上传回退）
        const MAX_ATTEMPTS: usize = 3;
        for attempt in 0..MAX_ATTEMPTS {
            let first_try = attempt == 0;
            match self
                .v0_chat_once(&req, &req.prompt, "", request_id, first_try)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(CoreError::Overloaded) => {
                    if attempt + 1 >= MAX_ATTEMPTS {
                        return Err(CoreError::Overloaded);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    log::warn!(
                        target: "ds_core::accounts",
                        "req={} 请求失败 (attempt {}/{}): {}",
                        request_id, attempt + 1, MAX_ATTEMPTS, e
                    );
                    if attempt + 1 >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            }
        }
        Err(CoreError::Overloaded)
    }

    /// 回退方案 A：历史文件上传（default / vision）
    async fn v0_chat_oversized_file(
        &self,
        req: &ChatRequest,
        request_id: &str,
    ) -> Result<ChatResponse, CoreError> {
        const MAX_ATTEMPTS: usize = 3;

        let (inline_prompt, history_content) = split_history_prompt(&req.prompt);

        if !history_content.is_empty() {
            log::debug!(
                target: "ds_core::accounts",
                "req={} 触发历史拆分, history_size={}", request_id, history_content.len()
            );
        }

        for attempt in 0..MAX_ATTEMPTS {
            let first_try = attempt == 0;
            match self
                .v0_chat_once(req, &inline_prompt, &history_content, request_id, first_try)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(CoreError::Overloaded) => {
                    if attempt + 1 >= MAX_ATTEMPTS {
                        return Err(CoreError::Overloaded);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    log::warn!(
                        target: "ds_core::accounts",
                        "req={} 请求失败 (attempt {}/{}): {}",
                        request_id, attempt + 1, MAX_ATTEMPTS, e
                    );
                    if attempt + 1 >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            }
        }
        Err(CoreError::Overloaded)
    }

    /// 回退方案 B：分块 completion 写入 session（expert，绕过文件上传限制）
    async fn v0_chat_oversized_chunk(
        &self,
        req: &ChatRequest,
        request_id: &str,
    ) -> Result<ChatResponse, CoreError> {
        // 1. 获取账号
        let guard = self
            .accounts
            .get_account_with_wait(30_000)
            .await
            .ok_or_else(|| {
                log::warn!(
                    target: "ds_core::accounts",
                    "req={} 账号池无可用账号", request_id
                );
                CoreError::Overloaded
            })?;
        let account = guard.account_arc();
        let account_id = account.display_id().to_string();
        let token = account.token().to_string();

        log::debug!(
            target: "ds_core::accounts",
            "req={} 分块写入: model_type=expert, account={}", request_id, account_id
        );

        // 2. 创建 session（所有 chunk 共享）
        let session_id = match self.accounts.create_session(&account).await {
            Ok(id) => id,
            Err(e) => {
                self.accounts.mark_error(&account_id);
                return Err(e);
            }
        };
        // 从这里起，任何提前返回都由守卫负责删除 session（兜底 `?` 传播的路径）
        let mut session_guard = SessionGuard::new(account.client(), &token, &session_id);

        // 3. 按 75% limit 切分 prompt
        let limit = self.input_character_limit_for(&req.model_type);
        let chunk_size = (limit as u64 * 75 / 100) as usize;
        let chunks = split_prompt_chunks(&req.prompt, chunk_size);

        // 4. Feed 非末 chunk 到 session
        let mut parent_message_id: Option<i64> = None;
        for (i, chunk) in chunks[..chunks.len() - 1].iter().enumerate() {
            let pow_header = match self
                .accounts
                .compute_pow_for_target(&account, "/api/v0/chat/completion")
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    self.accounts.mark_error(&account_id);
                    return Err(e);
                }
            };

            let payload = CompletionPayload {
                chat_session_id: session_id.clone(),
                parent_message_id,
                model_type: req.model_type.clone(),
                prompt: chunk.clone(),
                ref_file_ids: vec![],
                thinking_enabled: false,
                search_enabled: false,
                action: None,
                preempt: false,
            };

            let mut stream = match self
                .accounts
                .completion(&account, &pow_header, &payload)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    self.accounts.mark_error(&account_id);
                    return Err(e);
                }
            };

            // 等 ready + update_session
            let (stop_id, mut close_buf) =
                wait_ready_and_update(&mut stream, request_id, i + 1, chunks.len() - 1).await?;

            parent_message_id = Some(stop_id);

            // 发送停止信号（fire-and-forget）
            let stop_payload = crate::accounts::StopStreamPayload {
                chat_session_id: session_id.clone(),
                message_id: stop_id,
            };
            let _ = self.accounts.stop_stream(&account, &stop_payload).await;

            // 消费流直到 close 事件
            wait_close(
                &mut stream,
                &mut close_buf,
                request_id,
                i + 1,
                chunks.len() - 1,
            )
            .await?;

            log::debug!(
                target: "ds_core::accounts",
                "req={} 分块 {}/{} parent={:?}", request_id, i + 1, chunks.len() - 1, parent_message_id
            );
        }

        // 5. 末 chunk：正常 completion
        let last_chunk = chunks.into_iter().last().unwrap();
        let pow_header = match self
            .accounts
            .compute_pow_for_target(&account, "/api/v0/chat/completion")
            .await
        {
            Ok(h) => h,
            Err(e) => {
                self.accounts.mark_error(&account_id);
                return Err(e);
            }
        };

        let payload = CompletionPayload {
            chat_session_id: session_id.clone(),
            parent_message_id,
            model_type: req.model_type.clone(),
            prompt: last_chunk,
            ref_file_ids: vec![],
            thinking_enabled: req.thinking_enabled,
            search_enabled: req.search_enabled,
            action: None,
            preempt: false,
        };

        let mut raw_stream = match self
            .accounts
            .completion(&account, &pow_header, &payload)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.accounts.mark_error(&account_id);
                return Err(e);
            }
        };

        // 收集前两个 SSE 事件（ready + hint/update_session）
        let mut buf = Vec::new();
        let mut text_buf = String::new();
        let (ready_block, second_block) = loop {
            let chunk = raw_stream
                .next()
                .await
                .ok_or_else(|| {
                    let raw = String::from_utf8_lossy(&buf);
                    if let Some(biz_code) = raw
                        .lines()
                        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                        .and_then(|v| v.pointer("/data/biz_code").and_then(|c| c.as_i64()))
                    {
                        let biz_msg = raw
                            .lines()
                            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                            .and_then(|v| {
                                v.pointer("/data/biz_msg")
                                    .and_then(|m| m.as_str().map(String::from))
                            })
                            .unwrap_or_default();
                        log::error!(
                            target: "ds_core::accounts",
                            "req={} SSE 流返回业务错误: biz_code={}, biz_msg={}",
                            request_id, biz_code, biz_msg
                        );
                        self.accounts.mark_error(&account_id);
                        return CoreError::ProviderError(format!(
                            "biz_code={}, {}",
                            biz_code, biz_msg
                        ));
                    }
                    if raw.trim().starts_with('{') {
                        self.accounts.mark_error(&account_id);
                        return parse_json_error(&raw, request_id);
                    }
                    log::error!(
                        target: "ds_core::accounts",
                        "req={} 空 SSE 流, 已收到 {} 字节: {}", request_id, buf.len(), raw
                    );
                    CoreError::Stream(format!("空 SSE 流 (已收到 {} 字节)", buf.len()))
                })?
                .map_err(|e| CoreError::Stream(e.to_string()))?;
            log::trace!(
                target: "ds_core::accounts",
                "req={} <<< ({} bytes) {}", request_id, chunk.len(), String::from_utf8_lossy(&chunk)
            );
            buf.extend_from_slice(&chunk);
            text_buf.push_str(&String::from_utf8_lossy(&chunk));

            if let Some((first, second)) = split_two_events(&text_buf) {
                break (first.to_owned(), second.to_owned());
            }
        };

        let (_, stop_id) = parse_ready_message_ids(ready_block.as_bytes());

        // 检查 hint 事件
        if let Some(err) = check_hint(&second_block) {
            if let CoreError::Overloaded = &err {
                log::warn!(
                    target: "ds_core::accounts",
                    "req={} hint 限流: rate_limit_reached", request_id
                );
                self.accounts.mark_error(&account_id);
            } else {
                let hint_detail = second_block
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                    .and_then(|v| {
                        v.get("content")
                            .or_else(|| v.get("finish_reason"))
                            .and_then(|c| c.as_str().map(String::from))
                    })
                    .unwrap_or_else(|| "(unknown)".into());
                log::warn!(
                    target: "ds_core::accounts",
                    "req={} hint 错误: {}", request_id, hint_detail
                );
            }
            log::debug!(
                target: "ds_core::accounts",
                "req={} hint 后清理 session: id={}", request_id, session_id
            );
            return Err(err);
        }

        log::debug!(
            target: "ds_core::accounts",
            "req={} SSE ready: resp_msg={}", request_id, stop_id
        );

        // 注册活跃 session
        {
            let mut map = self.active_sessions.lock().unwrap();
            map.insert(
                session_id.clone(),
                ActiveSession {
                    client: account.client(),
                    token: token.clone(),
                    session_id: session_id.clone(),
                    message_id: stop_id,
                },
            );
        }

        // 用原始 buf 重建流
        let stream =
            futures::stream::once(futures::future::ready(Ok(Bytes::from(buf)))).chain(raw_stream);

        // session 生命周期移交给 SessionHandle（流结束时删除）
        session_guard.disarm();

        Ok(ChatResponse {
            stream: Box::pin(ResponseStream::new(
                Box::pin(stream),
                guard,
                SessionHandle {
                    client: account.client(),
                    token,
                    session_id,
                    message_id: stop_id,
                    sessions: self.active_sessions.clone(),
                },
                account_id.clone(),
            )),
        })
    }

    /// 单次请求尝试（不含重试逻辑）
    async fn v0_chat_once(
        &self,
        req: &ChatRequest,
        inline_prompt: &str,
        history_content: &str,
        request_id: &str,
        first_try: bool,
    ) -> Result<ChatResponse, CoreError> {
        // 1. 获取空闲账号
        let guard = if first_try {
            self.accounts.get_account_with_wait(30_000).await
        } else {
            self.accounts.get_account()
        }
        .ok_or_else(|| {
            log::warn!(
                target: "ds_core::accounts",
                "req={} 账号池无可用账号", request_id
            );
            CoreError::Overloaded
        })?;

        let account = guard.account_arc();
        let account_id = account.display_id().to_string();
        let token = account.token().to_string();

        log::debug!(
            target: "ds_core::accounts",
            "req={} 分配账号: model_type={}, account={}",
            request_id, req.model_type, account_id
        );

        // 2. 创建临时 session
        let session_start = Instant::now();
        let session_id = match self.accounts.create_session(&account).await {
            Ok(id) => id,
            Err(e) => {
                self.accounts.mark_error(&account_id);
                return Err(e);
            }
        };
        let session_create_ms = session_start.elapsed().as_millis();
        // 从这里起，任何提前返回都由守卫负责删除 session
        let mut session_guard = SessionGuard::new(account.client(), &token, &session_id);
        log::info!(
            target: "ds_core::accounts",
            "req={} session_created: id={}, create_ms={}, account={}",
            request_id, session_id, session_create_ms, account_id
        );

        // 3. 上传文件：先历史文件，再外部文件
        let mut ref_file_ids: Vec<String> = Vec::new();
        let mut history_upload_failed = false;

        if !history_content.is_empty() {
            match self
                .accounts
                .upload_and_poll(
                    &account,
                    SESSION_HISTORY_FILE,
                    "text/plain",
                    history_content.as_bytes(),
                    request_id,
                )
                .await
            {
                Ok(file_id) => ref_file_ids.push(file_id),
                Err(e) => {
                    log::warn!(
                        target: "ds_core::accounts",
                        "req={} 历史文件上传失败，退回内联发送: {}", request_id, e
                    );
                    history_upload_failed = true;
                }
            }
        }

        for file in &req.files {
            match self
                .accounts
                .upload_and_poll(
                    &account,
                    &file.filename,
                    &file.content_type,
                    &file.content,
                    request_id,
                )
                .await
            {
                Ok(file_id) => ref_file_ids.push(file_id),
                Err(e) => {
                    log::warn!(
                        target: "ds_core::accounts",
                        "req={} 外部文件上传失败 ({}): {}", request_id, file.filename, e
                    );
                    return Err(CoreError::ProviderError(format!(
                        "外部文件上传失败 ({}): {}",
                        file.filename, e
                    )));
                }
            }
        }

        // 4. 计算 PoW
        let pow_start = Instant::now();
        let pow_header = match self
            .accounts
            .compute_pow_for_target(&account, "/api/v0/chat/completion")
            .await
        {
            Ok(h) => h,
            Err(e) => {
                self.accounts.mark_error(&account_id);
                return Err(e);
            }
        };
        let pow_ms = pow_start.elapsed().as_millis();

        // 5. 发起 completion
        let completion_prompt: &str = if history_upload_failed {
            &req.prompt
        } else {
            inline_prompt
        };

        let payload = CompletionPayload {
            chat_session_id: session_id.clone(),
            parent_message_id: None,
            model_type: req.model_type.clone(),
            prompt: completion_prompt.to_string(),
            ref_file_ids,
            thinking_enabled: req.thinking_enabled,
            search_enabled: req.search_enabled,
            action: None,
            preempt: false,
        };

        let completion_start = Instant::now();
        let mut raw_stream = match self
            .accounts
            .completion(&account, &pow_header, &payload)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.accounts.mark_error(&account_id);
                return Err(e);
            }
        };

        // 6. 收集字节直到拿到前两个 SSE 事件（ready + hint/update_session）
        let mut buf = Vec::new();
        let mut text_buf = String::new();
        let (ready_block, second_block) = loop {
            let chunk = raw_stream
                .next()
                .await
                .ok_or_else(|| {
                    let raw = String::from_utf8_lossy(&buf);
                    if let Some(biz_code) = raw
                        .lines()
                        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                        .and_then(|v| v.pointer("/data/biz_code").and_then(|c| c.as_i64()))
                    {
                        let biz_msg = raw
                            .lines()
                            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                            .and_then(|v| {
                                v.pointer("/data/biz_msg")
                                    .and_then(|m| m.as_str().map(String::from))
                            })
                            .unwrap_or_default();
                        log::error!(
                            target: "ds_core::accounts",
                            "req={} SSE 流返回业务错误: biz_code={}, biz_msg={}",
                            request_id, biz_code, biz_msg
                        );
                        self.accounts.mark_error(&account_id);
                        return CoreError::ProviderError(format!(
                            "biz_code={}, {}",
                            biz_code, biz_msg
                        ));
                    }
                    log::error!(
                        target: "ds_core::accounts",
                        "req={} 空 SSE 流, 已收到 {} 字节: {}", request_id, buf.len(), raw
                    );
                    CoreError::Stream(format!("空 SSE 流 (已收到 {} 字节)", buf.len()))
                })?
                .map_err(|e| CoreError::Stream(e.to_string()))?;
            buf.extend_from_slice(&chunk);
            text_buf.push_str(&String::from_utf8_lossy(&chunk));

            if let Some((first, second)) = split_two_events(&text_buf) {
                break (first.to_owned(), second.to_owned());
            }
        };

        let (_, stop_id) = parse_ready_message_ids(ready_block.as_bytes());

        let ready_ms = completion_start.elapsed().as_millis();

        // 7. 检查 hint 事件
        if let Some(err) = check_hint(&second_block) {
            if let CoreError::Overloaded = &err {
                log::warn!(
                    target: "ds_core::accounts",
                    "req={} hint 限流: rate_limit_reached", request_id
                );
                self.accounts.mark_error(&account_id);
            } else {
                let hint_detail = second_block
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                    .and_then(|v| {
                        v.get("content")
                            .or_else(|| v.get("finish_reason"))
                            .and_then(|c| c.as_str().map(String::from))
                    })
                    .unwrap_or_else(|| "(unknown)".into());
                log::warn!(
                    target: "ds_core::accounts",
                    "req={} hint 错误: {}", request_id, hint_detail
                );
            }
            let hint_ms = completion_start.elapsed().as_millis();
            log::info!(
                target: "ds_core::accounts",
                "req={} hint_error_cleanup: session_id={}, pow_ms={}, hint_ms={}, session_create_ms={}, total_ms={}",
                request_id, session_id, pow_ms, hint_ms, session_create_ms, session_create_ms + hint_ms
            );
            return Err(err);
        }

        log::info!(
            target: "ds_core::accounts",
            "req={} sse_ready: resp_msg={}, pow_ms={}, completion_ms={}, session_create_ms={}, total_ms={}",
            request_id, stop_id, pow_ms, ready_ms, session_create_ms, session_create_ms + ready_ms
        );

        // 8. 注册活跃 session
        {
            let mut map = self.active_sessions.lock().unwrap();
            map.insert(
                session_id.clone(),
                ActiveSession {
                    client: account.client(),
                    token: token.clone(),
                    session_id: session_id.clone(),
                    message_id: stop_id,
                },
            );
        }

        // 9. 用原始 buf 重建流
        let stream =
            futures::stream::once(futures::future::ready(Ok(Bytes::from(buf)))).chain(raw_stream);

        // session 生命周期移交给 SessionHandle（流结束时删除）
        session_guard.disarm();

        Ok(ChatResponse {
            stream: Box::pin(ResponseStream::new(
                Box::pin(stream),
                guard,
                SessionHandle {
                    client: account.client(),
                    token,
                    session_id,
                    message_id: stop_id,
                    sessions: self.active_sessions.clone(),
                },
                account_id.clone(),
            )),
        })
    }
}

// ── ChatML 解析与历史拆分 ──────────────────────────────────────────────

/// 按 `<｜Role｜>` 标签边界切分 prompt 为 chunk
///
/// 贪心地把相邻的完整 message 块打包到 `chunk_size` 字符以内，避免字符级盲切
/// 把一个标签切成 `<｜Assista` / `nt｜>` 两半（上游会因此偶发空回复）。
/// 单个 message 本身就超过 `chunk_size` 时，退化为对该块做字符级切分。
fn split_prompt_chunks(prompt: &str, chunk_size: usize) -> Vec<String> {
    let char_chunks = |s: &str| -> Vec<String> {
        s.chars()
            .collect::<Vec<_>>()
            .chunks(chunk_size)
            .map(|c| c.iter().collect())
            .collect()
    };

    // 收集所有标签起点作为切分候选边界
    let mut tag_starts = Vec::new();
    let mut search_pos = 0;
    while let Some(idx) = prompt[search_pos..].find(TAG_START) {
        let abs = search_pos + idx;
        tag_starts.push(abs);
        search_pos = abs + TAG_START.len();
    }
    if tag_starts.is_empty() {
        return char_chunks(prompt);
    }

    // 按标签起点切块：每块从标签开始，到下一个标签起点结束
    let mut blocks: Vec<&str> = Vec::new();
    if tag_starts[0] > 0 {
        blocks.push(&prompt[..tag_starts[0]]);
    }
    for (i, start) in tag_starts.iter().enumerate() {
        let end = tag_starts.get(i + 1).copied().unwrap_or(prompt.len());
        blocks.push(&prompt[*start..end]);
    }

    // 贪心合并块，尽量塞满每个 chunk
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for block in blocks {
        let block_len = block.chars().count();
        if block_len > chunk_size {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            chunks.extend(char_chunks(block));
            continue;
        }
        if current.chars().count() + block_len > chunk_size && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(block);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

struct ChatBlock {
    role: String,
    content: String,
}

fn role_tag(role: &str) -> String {
    let mut r = role.to_string();
    if let Some(c) = r.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    format!("<｜{}｜>", r)
}

/// 解析 DeepSeek 原生标签格式的 prompt 为结构化块
fn parse_native_blocks(prompt: &str) -> Vec<ChatBlock> {
    let mut blocks = Vec::new();
    let mut pos = 0;
    while let Some(start_idx) = prompt[pos..].find(TAG_START) {
        let abs_start = pos + start_idx;
        let role_start = abs_start + TAG_START.len();
        let role_end = match prompt[role_start..].find(TAG_END) {
            Some(i) => role_start + i,
            None => break,
        };
        let role = prompt[role_start..role_end].trim().to_lowercase();
        let content_start = role_end + TAG_END.len();
        let content_end = prompt[content_start..]
            .find(TAG_START)
            .map_or(prompt.len(), |i| content_start + i);
        let content = prompt[content_start..content_end]
            .trim_end_matches('\n')
            .to_string();
        blocks.push(ChatBlock { role, content });
        pos = content_end;
    }
    blocks
}

/// 拆分 prompt 为 inline_prompt 和 history_content
///
/// 优先策略：找到最后一个 `<｜Assistant｜>` 块，
/// - inline = 仅该 assistant 块
/// - history = 其余所有块，包装为 [file content end] … [file content begin] 格式上传
fn split_history_prompt(prompt: &str) -> (String, String) {
    let blocks = parse_native_blocks(prompt);

    if let Some(ast_idx) = blocks.iter().rposition(|b| b.role == "assistant") {
        let mut inline = String::new();
        inline.push_str(&role_tag(&blocks[ast_idx].role));
        inline.push_str(&blocks[ast_idx].content);
        inline.push('\n');

        let mut history = String::new();
        history.push_str("[file content end]\n\n");
        for block in &blocks[..ast_idx] {
            history.push_str(&role_tag(&block.role));
            history.push_str(&block.content);
            history.push('\n');
        }
        history.push_str("[file name]: IGNORE\n[file content begin]\n");

        return (inline, history);
    }

    // 没有 assistant 块（理论不应发生），完整 prompt 内联
    (prompt.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个 chunk 都必须以完整标签开头，且不得把 `<｜Role｜>` 切成两半
    fn assert_tags_intact(chunks: &[String]) {
        for chunk in chunks {
            // 允许首块是标签前的内容；其余块必须从标签起点开始
            if chunk.contains("｜>") {
                let opens = chunk.matches("<｜").count();
                let closes = chunk.matches("｜>").count();
                assert_eq!(
                    opens,
                    closes,
                    "chunk 内标签不成对（半截标签）: {:?}",
                    chunk.chars().take(60).collect::<String>()
                );
            }
        }
    }

    #[test]
    fn split_prompt_chunks_respects_tag_boundaries() {
        let mut prompt = String::new();
        for i in 0..40 {
            prompt.push_str("<｜User｜>");
            prompt.push_str(&format!("这是第 {i} 条用户消息, 用来把 prompt 撑长一些。"));
            prompt.push_str("<｜Assistant｜>好的，收到。\n");
        }
        let chunks = split_prompt_chunks(&prompt, 500);
        assert!(chunks.len() > 1, "prompt 应被切分为多个 chunk");
        assert_tags_intact(&chunks);
        // 拼回原样（内容无损）
        assert_eq!(chunks.concat(), prompt);
        // 除首块外，每块都以标签开头
        for chunk in chunks.iter().skip(1) {
            assert!(
                chunk.starts_with(TAG_START),
                "chunk 未从标签边界开始: {:?}",
                chunk.chars().take(40).collect::<String>()
            );
        }
    }

    #[test]
    fn split_prompt_chunks_handles_oversized_single_block() {
        let prompt = format!("<｜User｜>{}", "长".repeat(1000));
        let chunks = split_prompt_chunks(&prompt, 100);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), prompt);
        // 单块超限时退化为字符切分，字符数不得超限
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 100);
        }
    }

    #[test]
    fn split_prompt_chunks_without_tags_falls_back_to_char_split() {
        let prompt = "a".repeat(250);
        let chunks = split_prompt_chunks(&prompt, 100);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), prompt);
    }

    #[test]
    fn split_prompt_chunks_respects_char_budget() {
        let prompt = "<｜User｜>你好<｜Assistant｜>你好呀<｜User｜>再见";
        let chunks = split_prompt_chunks(prompt, 12);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 12, "chunk 超限: {chunk:?}");
        }
        assert_eq!(chunks.concat(), prompt);
    }
}

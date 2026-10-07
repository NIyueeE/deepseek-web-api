//! 流处理模块 —— 精简响应协议与 DeepSeek SSE 解析
//!
//! 协议定义：将 DeepSeek 原始 SSE 字节流（p/o/v patch 协议）转换为结构化事件序列，
//! 主 crate 只需消费 StreamEvent，不再感知底层格式。

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::{Buf, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use pin_project_lite::pin_project;

use crate::CoreError;
use crate::accounts::ClientError;
use crate::accounts::{AccountGuard, DsClient, StopStreamPayload};

// ── 精简响应协议 ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Meta {
        account_id: String,
    },
    ThinkStart,
    ThinkDelta {
        content: String,
    },
    ContentStart,
    ContentDelta {
        content: String,
    },
    Done {
        finish_reason: Option<String>,
        accumulated_token_usage: Option<u32>,
    },
}

// ── 内部类型 ──────────────────────────────────────────────────────────

pub(crate) struct ActiveSession {
    /// 该账号设备身份下的客户端视图（shutdown 清理时需要一致的 X-Device-Id）
    pub(crate) client: DsClient,
    pub(crate) token: String,
    pub(crate) session_id: String,
    pub(crate) message_id: i64,
}

pub(crate) struct SessionHandle {
    pub(crate) client: DsClient,
    pub(crate) token: String,
    pub(crate) session_id: String,
    pub(crate) message_id: i64,
    pub(crate) sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
}

impl SessionHandle {
    fn cleanup(&self, finished: bool) {
        let start = Instant::now();
        self.sessions.lock().unwrap().remove(&self.session_id);
        let client = self.client.clone();
        let token = self.token.clone();
        let session_id = self.session_id.clone();
        let message_id = self.message_id;

        tokio::spawn(async move {
            if !finished {
                let payload = StopStreamPayload {
                    chat_session_id: session_id.clone(),
                    message_id,
                };
                if let Err(e) = client.stop_stream(&token, &payload).await {
                    log::warn!(target: "ds_core::accounts", "stop_stream failed: {e}");
                }
            }
            if let Err(e) = client.delete_session(&token, &session_id).await {
                log::warn!(target: "ds_core::accounts", "delete_session failed: {e}");
            }
            log::info!(
                target: "ds_core::accounts",
                "session_deleted: id={}, finished={}, cleanup_ms={}",
                session_id, finished, start.elapsed().as_millis()
            );
        });
    }
}

// ── DeepSeek Patch 状态机 ─────────────────────────────────────────────

const FRAG_THINK: &str = "THINK";
const FRAG_RESPONSE: &str = "RESPONSE";

/// Fragment 状态
///
/// 只记录类型：增量内容随事件**转移**给下游（下游是唯一读取方），
/// 不再逐条 `push_str` 累积 —— 长响应下可省下与响应等长的内存与拷贝。
struct Fragment {
    ty: String,
}

/// 维护 DeepSeek 响应的 patch 状态，对齐前端 DeltaParser
///
/// - `p` / `o` 跨事件持久化
/// - `o` 默认 "SET"
struct PatchState {
    current_path: Option<String>,
    current_op: Option<String>,
    fragments: Vec<Fragment>,
    status: Option<String>,
    accumulated_token_usage: Option<u32>,
    /// 是否已产出过非空 RESPONSE 内容（FINISHED 无内容告警用，避免缓存全文）
    response_has_content: bool,
}

impl PatchState {
    const fn new() -> Self {
        Self {
            current_path: None,
            current_op: None,
            fragments: Vec::new(),
            status: None,
            accumulated_token_usage: None,
            response_has_content: false,
        }
    }

    /// 消费一帧 SSE 文本，返回零个或多个 StreamEvent
    fn apply_frame(&mut self, frame: &str) -> Result<Vec<StreamEvent>, CoreError> {
        if frame.is_empty() {
            return Ok(Vec::new());
        }

        // 提取 event 和 data 行
        let event_type = frame
            .lines()
            .find_map(|l| l.trim().strip_prefix("event:"))
            .map(|v| v.trim());

        let data = frame
            .lines()
            .find_map(|l| l.trim().strip_prefix("data:"))
            .map(|v| v.trim());

        let mut events = Vec::new();

        // event: ready → 不产出事件（在初始化阶段已处理）
        // event: hint → 检查错误
        if event_type == Some("hint")
            && let Some(data) = data
        {
            return Err(hint_to_error(data));
        }

        // 解析 data JSON
        if let Some(data) = data
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(data)
        {
            events.extend(self.apply_patch(val));
        }

        Ok(events)
    }

    /// 应用 p/o/v patch
    ///
    /// `val` 按值传入并原地取用：JSON 里的增量字符串直接**移出**交给下游事件，
    /// 避免「解析出一份 + 再拷贝一份」的双份分配。
    fn apply_patch(&mut self, mut val: serde_json::Value) -> Vec<StreamEvent> {
        // p/o 跨事件持久化
        if let Some(p) = val.get("p").and_then(|v| v.as_str()) {
            self.current_path = Some(p.to_string());
        }
        if let Some(o) = val.get("o").and_then(|v| v.as_str()) {
            self.current_op = Some(o.to_string());
        }

        let op = self.current_op.as_deref().unwrap_or("SET").to_string();
        let path = self.current_path.as_deref().unwrap_or("").to_string();

        let Some(v) = val.get_mut("v") else {
            return Vec::new();
        };

        // 初始快照：无 path 且 v 含 response
        if self.current_path.is_none()
            && let Some(response) = v.get_mut("response")
        {
            return self.apply_initial_snapshot(response);
        }

        // BATCH 递归分解
        if op == "BATCH" && v.is_array() {
            return self.apply_batch(&path, v);
        }

        self.apply_path(&path, &op, v)
    }

    fn apply_batch(&mut self, parent_path: &str, arr: &mut serde_json::Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let Some(arr) = arr.as_array_mut() else {
            return events;
        };

        // 子解析器独立状态
        let (mut sub_path, mut sub_op) = (String::new(), String::from("SET"));

        for item in arr {
            if let Some(p) = item.get("p").and_then(|v| v.as_str()) {
                sub_path = p.to_string();
            }
            if let Some(o) = item.get("o").and_then(|v| v.as_str()) {
                sub_op = o.to_string();
            }

            let Some(v) = item.get_mut("v") else {
                continue;
            };

            let full_path = if parent_path.is_empty() {
                sub_path.clone()
            } else if sub_path.is_empty() {
                parent_path.to_string()
            } else {
                format!("{parent_path}/{sub_path}")
            };

            if sub_op == "BATCH" {
                let mut batch_events = self.apply_batch(&full_path, v);
                events.append(&mut batch_events);
            } else {
                let mut path_events = self.apply_path(&full_path, &sub_op, v);
                events.append(&mut path_events);
            }
        }

        events
    }

    fn apply_initial_snapshot(&mut self, response: &mut serde_json::Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if let Some(s) = response.get("status").and_then(|v| v.as_str()) {
            self.status = Some(s.to_string());
        }

        if let Some(n) = response
            .get("accumulated_token_usage")
            .and_then(|v| v.as_u64())
        {
            self.accumulated_token_usage = Some(u32::try_from(n).unwrap_or(u32::MAX));
        }

        if let Some(arr) = response.get_mut("fragments").and_then(|f| f.as_array_mut()) {
            self.fragments.clear();
            for frag in arr {
                let Some(obj) = frag.as_object_mut() else {
                    continue;
                };
                let Some(ty) = obj.remove("type").and_then(take_string_owned) else {
                    continue;
                };
                let content = obj
                    .get_mut("content")
                    .and_then(take_string)
                    .unwrap_or_default();
                let is_think = ty == FRAG_THINK;
                let is_response = ty == FRAG_RESPONSE;
                if is_response && !content.is_empty() {
                    self.response_has_content = true;
                }
                if !content.is_empty() {
                    if is_think {
                        events.push(StreamEvent::ThinkDelta { content });
                    } else if is_response {
                        events.push(StreamEvent::ContentDelta { content });
                    }
                }
                self.fragments.push(Fragment { ty });
            }
        }

        events
    }

    fn apply_path(
        &mut self,
        path: &str,
        op: &str,
        val: &mut serde_json::Value,
    ) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        match path {
            "response/status" | "/response/status" => {
                if let Some(s) = take_string(val) {
                    self.status = Some(s);
                }
            }
            "response/accumulated_token_usage"
            | "accumulated_token_usage"
            | "/response/accumulated_token_usage"
            | "/accumulated_token_usage" => {
                if let Some(n) = val.as_u64() {
                    self.accumulated_token_usage = Some(u32::try_from(n).unwrap_or(u32::MAX));
                }
            }
            "response/fragments/-1/content" | "/response/fragments/-1/content" => {
                if let Some(s) = take_string(val)
                    && let Some(frag) = self.fragments.last()
                {
                    match frag.ty.as_str() {
                        FRAG_THINK => events.push(StreamEvent::ThinkDelta { content: s }),
                        FRAG_RESPONSE => {
                            self.response_has_content = true;
                            events.push(StreamEvent::ContentDelta { content: s });
                        }
                        _ => {}
                    }
                }
            }
            "response/fragments" | "/response/fragments" if op == "APPEND" => {
                if let Some(arr) = val.as_array_mut() {
                    for item in arr {
                        let Some(obj) = item.as_object_mut() else {
                            continue;
                        };
                        let Some(ty) = obj.remove("type").and_then(take_string_owned) else {
                            continue;
                        };
                        let content = obj
                            .get_mut("content")
                            .and_then(take_string)
                            .unwrap_or_default();
                        let is_think = ty == FRAG_THINK;
                        let is_response = ty == FRAG_RESPONSE;
                        if !content.is_empty() {
                            if is_think {
                                events.push(StreamEvent::ThinkDelta { content });
                            } else if is_response {
                                self.response_has_content = true;
                                events.push(StreamEvent::ContentDelta { content });
                            }
                        }
                        self.fragments.push(Fragment { ty });
                    }
                }
            }
            _ => {}
        }

        events
    }
}

/// 取出 JSON 字符串的所有权（内容不复制）
fn take_string(val: &mut serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => Some(std::mem::take(s)),
        _ => None,
    }
}

/// 取出 `serde_json::Value` 内部字符串的所有权
fn take_string_owned(val: serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    }
}

/// SSE 帧字节 → 文本
///
/// 帧内是 UTF-8 JSON：合法时**借用**帧缓冲（零拷贝），
/// 仅在非法字节序列时退化为有损解码。
fn frame_str(frame: &[u8]) -> Cow<'_, str> {
    std::str::from_utf8(frame).map_or_else(|_| String::from_utf8_lossy(frame), Cow::Borrowed)
}

// ── 响应阶段跟踪 ─────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Phase {
    Init,
    Thinking,
    Content,
    Done,
}

pin_project! {
    pub(crate) struct ResponseStream {
        raw: Pin<Box<dyn Stream<Item = Result<Bytes, ClientError>> + Send>>,
        _guard: AccountGuard,
        session: SessionHandle,

        buf: BytesMut,
        patch_state: PatchState,
        phase: Phase,
        pending: VecDeque<StreamEvent>,
        meta_sent: bool,
        account_id: String,
        finished: bool,
    }

    impl PinnedDrop for ResponseStream {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            this.session.cleanup(*this.finished);
        }
    }
}

impl ResponseStream {
    pub(crate) fn new(
        raw: Pin<Box<dyn Stream<Item = Result<Bytes, ClientError>> + Send>>,
        guard: AccountGuard,
        session: SessionHandle,
        account_id: String,
    ) -> Self {
        Self {
            raw,
            _guard: guard,
            session,
            buf: BytesMut::new(),
            patch_state: PatchState::new(),
            phase: Phase::Init,
            pending: VecDeque::new(),
            meta_sent: false,
            account_id,
            finished: false,
        }
    }
}

impl Stream for ResponseStream {
    type Item = Result<StreamEvent, CoreError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        // 首个事件始终是 Meta
        if !*this.meta_sent {
            *this.meta_sent = true;
            return Poll::Ready(Some(Ok(StreamEvent::Meta {
                account_id: this.account_id.clone(),
            })));
        }

        // 先清空 pending 队列
        if let Some(evt) = this.pending.pop_front() {
            return Poll::Ready(Some(Ok(evt)));
        }

        if *this.phase == Phase::Done {
            return Poll::Ready(None);
        }

        loop {
            if let Some(frame) = take_frame(this.buf) {
                let events = match this.patch_state.apply_frame(frame_str(&frame).as_ref()) {
                    Ok(evts) => evts,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                };

                if events.is_empty() {
                    continue;
                }

                // 过滤：阶段切换信号插入 + 延后排队
                let mut filtered = VecDeque::new();
                finalize_events(this.patch_state, this.phase, events, &mut filtered);

                // 第一个事件立即返回（移动而非克隆），其余入 pending
                if let Some(first) = filtered.pop_front() {
                    this.pending.extend(filtered);
                    return Poll::Ready(Some(Ok(first)));
                }
            }

            match this.raw.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.buf.extend_from_slice(&bytes);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(CoreError::Stream(e.to_string()))));
                }
                Poll::Ready(None) => {
                    *this.finished = true;

                    // 冲刷缓冲区中剩余数据。上游在 status=FINISHED 之后偶发直接断流，
                    // 最后几帧（含 accumulated_token_usage / response/status）可能不带
                    // 结尾空行；必须在这里解析，否则会丢掉 finish_reason 与 token 用量。
                    let mut flushed = VecDeque::new();
                    loop {
                        let frame = if let Some(f) = take_frame(this.buf) {
                            f
                        } else if this.buf.is_empty() {
                            break;
                        } else {
                            // 取走剩余字节（O(1) 切分），交给帧解析
                            this.buf.split()
                        };
                        let events = this.patch_state.apply_frame(frame_str(&frame).as_ref())?;
                        // 即使 events 为空也要走一遍：status→Done 的转换在这里发生
                        finalize_events(this.patch_state, this.phase, events, &mut flushed);
                    }
                    // 缓冲区已空但状态机尚未结束：用已知的 status/usage 收尾
                    if *this.phase != Phase::Done {
                        finalize_events(this.patch_state, this.phase, Vec::new(), &mut flushed);
                    }

                    if let Some(first) = flushed.pop_front() {
                        this.pending.extend(flushed);
                        return Poll::Ready(Some(Ok(first)));
                    }

                    // 既无残留内容、也未收到结束信号：补发空 Done 让下游正常收尾
                    if *this.phase != Phase::Done {
                        *this.phase = Phase::Done;
                        return Poll::Ready(Some(Ok(StreamEvent::Done {
                            finish_reason: None,
                            accumulated_token_usage: None,
                        })));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ── SSE 帧提取 ───────────────────────────────────────────────────────

/// 取出一个完整 SSE 帧（不含分隔的 `\n\n`）
///
/// 用 `BytesMut::split_to` 做 O(1) 指针切分：不搬运剩余字节，也不做 UTF-8 拷贝。
fn take_frame(buf: &mut BytesMut) -> Option<BytesMut> {
    let pos = buf.windows(2).position(|w| w == b"\n\n")?;
    let frame = buf.split_to(pos);
    buf.advance(2);
    Some(frame)
}

fn hint_to_error(data: &str) -> CoreError {
    let val: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
    let content = val
        .get("content")
        .or_else(|| val.get("finish_reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");

    if content.contains("rate_limit") {
        CoreError::Overloaded
    } else if content.contains("input_exceeds_limit") {
        CoreError::ProviderError("输入内容超长，请缩短后重试".into())
    } else {
        CoreError::ProviderError(format!("hint: {content}"))
    }
}

// ── 内部辅助（供 ResponseStream 使用） ─────────────────────────────────

/// 对一帧解析出的事件做后处理：插入阶段切换信号，并在 status 结束时追加 Done
///
/// 正常循环与「流意外结束（EOF）时冲刷缓冲区」两条路径共用，
/// 避免 EOF 路径丢失上游已经下发的 `finish_reason` 与 `accumulated_token_usage`。
fn finalize_events(
    patch_state: &PatchState,
    phase: &mut Phase,
    events: Vec<StreamEvent>,
    out: &mut VecDeque<StreamEvent>,
) {
    for evt in events {
        match &evt {
            StreamEvent::ThinkDelta { .. } if *phase == Phase::Init || *phase == Phase::Content => {
                *phase = Phase::Thinking;
                out.push_back(StreamEvent::ThinkStart);
            }
            StreamEvent::ContentDelta { .. }
                if *phase == Phase::Init || *phase == Phase::Thinking =>
            {
                *phase = Phase::Content;
                out.push_back(StreamEvent::ContentStart);
            }
            _ => {}
        }
        out.push_back(evt);
    }

    if let Some(status) = &patch_state.status
        && (status == "FINISHED" || status == "INCOMPLETE")
        && *phase != Phase::Done
    {
        *phase = Phase::Done;
        let finish = (status == "FINISHED").then(|| "stop".to_string());
        let usage = patch_state.accumulated_token_usage;
        if finish.is_some() && !has_response_content(patch_state) {
            log::warn!(
                target: "ds_core::accounts",
                "状态机 FINISHED 但无 RESPONSE 内容"
            );
        }
        out.push_back(StreamEvent::Done {
            finish_reason: finish,
            accumulated_token_usage: usage,
        });
    }
}

/// 检查是否有非空的 RESPONSE 内容（用于 FINISHED 告警）
const fn has_response_content(state: &PatchState) -> bool {
    state.response_has_content
}

// ── 公开辅助（供 request.rs 初始化阶段使用） ──────────────────────────

pub(crate) fn split_two_events(buf: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = buf.splitn(3, "\n\n").collect();
    (parts.len() >= 3).then(|| (parts[0], parts[1]))
}

pub(crate) fn check_hint(event_block: &str) -> Option<CoreError> {
    let is_hint = event_block.lines().any(|l| {
        l.trim()
            .strip_prefix("event:")
            .is_some_and(|v| v.trim() == "hint")
    });
    if !is_hint {
        return None;
    }
    if event_block.contains("rate_limit") {
        return Some(CoreError::Overloaded);
    }
    if event_block.contains("input_exceeds_limit") {
        return Some(CoreError::ProviderError(
            "输入内容超长，请缩短后重试".into(),
        ));
    }
    None
}

pub(crate) fn parse_ready_message_ids(chunk: &[u8]) -> (i64, i64) {
    let text = std::str::from_utf8(chunk).ok();
    if let Some(text) = text {
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(data)
                && let (Some(r), Some(s)) = (
                    val.get("request_message_id").and_then(|v| v.as_i64()),
                    val.get("response_message_id").and_then(|v| v.as_i64()),
                )
            {
                return (r, s);
            }
        }
    }
    (1, 2)
}

pub(crate) fn parse_json_error(text: &str, request_id: &str) -> CoreError {
    let raw = text.trim();
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw)
        && let Some(code) = val.get("code").and_then(|c| c.as_i64())
    {
        let msg = val
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        log::error!(
            target: "ds_core::accounts",
            "req={request_id} JSON 错误响应: code={code}, msg={msg}"
        );
        return match code {
            1001 | 1201 => CoreError::Overloaded,
            40301 => CoreError::ProviderError(format!("INVALID_POW_RESPONSE: {msg}")),
            _ => CoreError::ProviderError(format!("API error code={code}: {msg}")),
        };
    }
    log::error!(
        target: "ds_core::accounts",
        "req={} 无法解析的响应: {}", request_id, raw.chars().take(200).collect::<String>()
    );
    CoreError::Stream(format!(
        "无法解析的响应: {}",
        raw.chars().take(200).collect::<String>()
    ))
}

pub(crate) async fn wait_ready_and_update(
    stream: &mut Pin<Box<dyn Stream<Item = Result<Bytes, ClientError>> + Send>>,
    request_id: &str,
    chunk_index: usize,
    total_chunks: usize,
) -> Result<(i64, Vec<u8>), CoreError> {
    let mut buf = Vec::new();
    let mut ready_msg_id: Option<i64> = None;
    loop {
        let chunk = stream
            .next()
            .await
            .ok_or_else(|| {
                let raw = String::from_utf8_lossy(&buf);
                if raw.trim().starts_with('{') {
                    return parse_json_error(&raw, request_id);
                }
                CoreError::Stream(format!(
                    "req={request_id} 分块 {chunk_index}/{total_chunks} 收到空流"
                ))
            })?
            .map_err(|e| CoreError::Stream(e.to_string()))?;
        buf.extend_from_slice(&chunk);
        let text = String::from_utf8_lossy(&buf);

        let events: Vec<&str> = text.split("\n\n").collect();
        let n_complete = if text.ends_with("\n\n") {
            events.len()
        } else {
            events.len().saturating_sub(1)
        };

        for event in &events[..n_complete] {
            if event.is_empty() {
                continue;
            }
            if let Some(err) = check_hint(event) {
                return Err(err);
            }
            if event.lines().any(|l| {
                l.trim()
                    .strip_prefix("event:")
                    .is_some_and(|v| v.trim() == "ready")
            }) {
                ready_msg_id = Some(parse_ready_message_ids(event.as_bytes()).1);
            }
            if let Some(id) = ready_msg_id
                && event.lines().any(|l| {
                    l.trim()
                        .strip_prefix("event:")
                        .is_some_and(|v| v.trim() == "update_session")
                })
            {
                return Ok((id, buf));
            }
        }
    }
}

pub(crate) async fn wait_close(
    stream: &mut Pin<Box<dyn Stream<Item = Result<Bytes, ClientError>> + Send>>,
    buf: &mut Vec<u8>,
    request_id: &str,
    chunk_index: usize,
    total_chunks: usize,
) -> Result<(), CoreError> {
    loop {
        let text = String::from_utf8_lossy(buf);
        let events: Vec<&str> = text.split("\n\n").collect();
        let n_complete = if text.ends_with("\n\n") {
            events.len()
        } else {
            events.len().saturating_sub(1)
        };

        for event in &events[..n_complete] {
            if event.lines().any(|l| {
                l.trim()
                    .strip_prefix("event:")
                    .is_some_and(|v| v.trim() == "close")
            }) {
                return Ok(());
            }
        }

        let chunk = stream
            .next()
            .await
            .ok_or_else(|| {
                CoreError::Stream(format!(
                    "req={request_id} 分块 {chunk_index}/{total_chunks} 流在 close 前结束"
                ))
            })?
            .map_err(|e| CoreError::Stream(e.to_string()))?;
        buf.extend_from_slice(&chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：上游在 status=FINISHED 之后直接断流时，`accumulated_token_usage` 与
    /// `finish_reason` 仍必须出现在 Done 事件里（issue #93：completion_tokens 恒为 0）
    #[test]
    fn finalize_events_keeps_usage_when_status_finished() {
        let mut state = PatchState::new();
        state.status = Some("FINISHED".to_string());
        state.accumulated_token_usage = Some(87);
        let mut phase = Phase::Content;
        let mut out = VecDeque::new();

        finalize_events(&state, &mut phase, Vec::new(), &mut out);

        assert_eq!(phase, Phase::Done);
        match out.make_contiguous() {
            [
                StreamEvent::Done {
                    finish_reason,
                    accumulated_token_usage,
                },
            ] => {
                assert_eq!(finish_reason.as_deref(), Some("stop"));
                assert_eq!(*accumulated_token_usage, Some(87));
            }
            other => panic!("期望单个 Done 事件，实际: {:?}", other.len()),
        }
    }

    /// INCOMPLETE 不应伪装成正常结束，但仍需带上 usage
    #[test]
    fn finalize_events_incomplete_has_no_finish_reason() {
        let mut state = PatchState::new();
        state.status = Some("INCOMPLETE".to_string());
        state.accumulated_token_usage = Some(12);
        let mut phase = Phase::Content;
        let mut out = VecDeque::new();

        finalize_events(&state, &mut phase, Vec::new(), &mut out);

        match out.make_contiguous() {
            [
                StreamEvent::Done {
                    finish_reason,
                    accumulated_token_usage,
                },
            ] => {
                assert_eq!(*finish_reason, None);
                assert_eq!(*accumulated_token_usage, Some(12));
            }
            other => panic!("期望单个 Done 事件，实际: {:?}", other.len()),
        }
    }

    /// 已经进入 Done 阶段后不得重复追加 Done（两条路径共用 finalize 时的去重保证）
    #[test]
    fn finalize_events_does_not_duplicate_done() {
        let mut state = PatchState::new();
        state.status = Some("FINISHED".to_string());
        let mut phase = Phase::Done;
        let mut out = VecDeque::new();

        finalize_events(&state, &mut phase, Vec::new(), &mut out);

        assert!(out.is_empty(), "Done 阶段不应再追加事件: {out:?}");
    }

    /// 阶段切换信号与 Done 的相对顺序：ContentStart 先于 Done，且 Done 只出现一次
    #[test]
    fn finalize_events_inserts_content_start_before_done() {
        let mut state = PatchState::new();
        state.status = Some("FINISHED".to_string());
        state.accumulated_token_usage = Some(5);
        let mut phase = Phase::Init;
        let mut out = VecDeque::new();

        finalize_events(
            &state,
            &mut phase,
            vec![StreamEvent::ContentDelta {
                content: "hi".into(),
            }],
            &mut out,
        );

        assert!(matches!(&out[0], StreamEvent::ContentStart));
        assert!(matches!(&out[1], StreamEvent::ContentDelta { .. }));
        assert!(matches!(&out[2], StreamEvent::Done { .. }));
        assert_eq!(out.len(), 3);
    }
}

//! OpenAI 响应转换 —— 将 StreamEvent 流映射为 OpenAI 响应格式
//!
//! 数据流：converter -> tool_parser -> repair -> stop_detect
//! - 仅 THINK / RESPONSE 片段映射到用户可见文本
//! - obfuscation 在最终 SSE 序列化阶段动态注入

mod converter;
mod tool_parser;

pub(crate) use tool_parser::{TOOL_CALL_END, TOOL_CALL_START, TagConfig};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::{debug, info, trace, warn};
use pin_project_lite::pin_project;
use rand::RngExt;
use tokio::time::Sleep;

use ds_core::StreamEvent;

use crate::openai_adapter::{
    OpenAIAdapterError,
    types::{
        ChatCompletionsResponse, ChatCompletionsResponseChunk, Choice, ChunkChoice, Delta,
        MessageResponse, ToolCall, Usage,
    },
};

static CHATCMPL_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_chatcmpl_id() -> String {
    let n = CHATCMPL_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("chatcmpl-{n:016x}")
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const OBFUSCATION_TARGET_LEN: usize = 512;
const OBFUSCATION_MIN_PAD: usize = 16;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
const FINISH_STOP: &str = "stop";
const FINISH_TOOL_CALLS: &str = "tool_calls";

fn random_padding(len: usize) -> String {
    if len == 0 {
        return String::new();
    }
    let byte_len = (len * 3).div_ceil(4);
    let mut bytes = vec![0u8; byte_len];
    rand::rng().fill(&mut bytes);
    let s = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    s[..len].to_string()
}

pub(crate) fn sse_serialize(
    chunk: &ChatCompletionsResponseChunk,
) -> Result<Bytes, OpenAIAdapterError> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut buf, chunk).map_err(OpenAIAdapterError::from)?;
    buf.extend_from_slice(b"\n\n");
    Ok(Bytes::from(buf))
}

fn find_stop_pos(content: &str, stop: &[String]) -> Option<usize> {
    stop.iter().filter_map(|s| content.find(s)).min()
}

/// 返回 `<= idx` 的最近 char 边界（等价于 nightly 的 `str::floor_char_boundary`）
///
/// 多语言输出（中文 / 日文 / 俄文）下 stop 串的字节位置可能落在 UTF-8
/// 续字节上，直接切片会 panic。
const fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// RepairStream 内部使用的流类型
type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>>;

/// 工具调用修复闭包类型
pub(crate) type RepairFn = Arc<
    dyn Fn(
            String,
        )
            -> Pin<Box<dyn Future<Output = Result<Vec<ToolCall>, OpenAIAdapterError>> + Send>>
        + Send
        + Sync,
>;

/// 执行 tool_calls 修复：将 StreamEvent 流中的 ContentDelta 提取为结构化 ToolCall
pub(crate) async fn execute_tool_repair(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent, ds_core::CoreError>> + Send>>,
    tag_config: &TagConfig,
) -> Result<Vec<ToolCall>, OpenAIAdapterError> {
    use futures::StreamExt;
    let mut stream = stream;

    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event.map_err(OpenAIAdapterError::from)? {
            StreamEvent::ContentDelta { content } => {
                text.push_str(&content);
                if text.len() > tool_parser::MAX_XML_BUF_LEN {
                    return Err(OpenAIAdapterError::Internal(
                        "修复模型输出过长，放弃修复".into(),
                    ));
                }
            }
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    let wrapped = if tool_parser::contains_start_tag_with(&text, tag_config) {
        text.trim().to_string()
    } else {
        format!(
            "{}{}{}",
            tool_parser::TOOL_CALL_START,
            text.trim(),
            tool_parser::TOOL_CALL_END
        )
    };

    let (calls, _) = tool_parser::parse_tool_calls_with(&wrapped, tag_config).ok_or_else(|| {
        OpenAIAdapterError::Internal(format!(
            "修复模型返回无法解析为工具调用: {}",
            &text[..floor_char_boundary(&text, 200)]
        ))
    })?;

    // 修复模型可能返回空结果，提前检查
    let trimmed = text.trim();
    if trimmed == "[]" || trimmed == "{}" {
        return Err(OpenAIAdapterError::Internal("修复模型返回空结果".into()));
    }
    Ok(calls)
}

enum RepairState {
    Forwarding,
    Repairing {
        future: Pin<Box<dyn Future<Output = Result<Vec<ToolCall>, OpenAIAdapterError>> + Send>>,
    },
    RepairFailed(String),
    Done,
}

pin_project! {
    /// 工具调用修复流：在 ToolCallStream 之后、StopDetectStream 之前
    ///
    /// 当 ToolCallStream 返回 Err(ToolCallRepairNeeded) 时，
    /// 丢弃上游流（释放账号），通过 repair_fn 发起修复请求，
    /// 将修复后的 tool_calls 发送给客户端。
    struct RepairStream {
        #[pin]
        inner: Option<ChunkStream>,
        repair_fn: Option<RepairFn>,
        state: RepairState,
        model: String,
        #[pin]
        keepalive_deadline: Sleep,
    }
}

impl RepairStream {
    fn new(inner: ChunkStream, repair_fn: RepairFn, model: String) -> Self {
        Self {
            inner: Some(inner),
            repair_fn: Some(repair_fn),
            state: RepairState::Forwarding,
            model,
            keepalive_deadline: tokio::time::sleep_until(
                tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
            ),
        }
    }
}

impl Stream for RepairStream {
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            match this.state {
                RepairState::Forwarding => {
                    match this.inner.as_mut().as_pin_mut().map(|p| p.poll_next(cx)) {
                        Some(Poll::Ready(Some(Ok(chunk)))) => {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        Some(Poll::Ready(Some(Err(OpenAIAdapterError::ToolCallRepairNeeded(
                            tool_text,
                        ))))) => {
                            warn!(
                                target: "adapter",
                                "RepairStream 捕获修复请求: len={}",
                                tool_text.len()
                            );
                            trace!(target: "adapter", ">>> repair: accepting tool_text len={}", tool_text.len());
                            drop(this.inner.as_mut().get_mut().take());
                            if let Some(f) = this.repair_fn.take() {
                                let future = f(tool_text);
                                *this.state = RepairState::Repairing { future };
                            } else {
                                *this.state =
                                    RepairState::RepairFailed("no repair function".into());
                            }
                            continue;
                        }
                        Some(Poll::Ready(Some(Err(e)))) => {
                            return Poll::Ready(Some(Err(e)));
                        }
                        Some(Poll::Ready(None)) | None => {
                            return Poll::Ready(None);
                        }
                        Some(Poll::Pending) => {
                            return Poll::Pending;
                        }
                    }
                }

                RepairState::Repairing { future } => match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(calls)) => {
                        info!(
                            target: "adapter",
                            "tool_calls 修复成功: {} 个工具调用",
                            calls.len()
                        );
                        trace!(target: "adapter", ">>> repair: success {} calls", calls.len());
                        *this.state = RepairState::Done;
                        return Poll::Ready(Some(Ok(converter::make_chunk(
                            this.model,
                            Delta {
                                tool_calls: Some(calls),
                                ..Default::default()
                            },
                            Some(FINISH_TOOL_CALLS),
                        ))));
                    }
                    Poll::Ready(Err(e)) => {
                        warn!(target: "adapter", "tool_calls repair failed: {e}");
                        *this.state = RepairState::RepairFailed(format!("修复失败: {e}"));
                        continue;
                    }
                    Poll::Pending => {
                        if this.keepalive_deadline.as_mut().poll(cx).is_ready() {
                            // 修复期间的心跳不得伪造 tool_call：客户端会按 index/id
                            // 累积，把空调用当成新工具调用（issue #87）。
                            // 空 delta 是协议合法的 no-op，客户端会忽略。
                            trace!(target: "adapter", ">>> keepalive(repair): 发送空 delta 心跳");
                            this.keepalive_deadline
                                .as_mut()
                                .reset(tokio::time::Instant::now() + KEEPALIVE_INTERVAL);
                            return Poll::Ready(Some(Ok(ChatCompletionsResponseChunk {
                                id: "chatcmpl-keepalive".into(),
                                object: "chat.completion.chunk",
                                created: 0,
                                model: this.model.clone(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: Delta::default(),
                                    finish_reason: None,
                                    logprobs: None,
                                }],
                                usage: None,
                                service_tier: None,
                                system_fingerprint: None,
                                obfuscation: None,
                            })));
                        }
                        return Poll::Pending;
                    }
                },

                RepairState::RepairFailed(msg) => {
                    let msg = std::mem::take(msg);
                    return Poll::Ready(Some(Err(OpenAIAdapterError::Internal(msg))));
                }

                RepairState::Done => return Poll::Ready(None),
            }
        }
    }
}

pin_project! {
    struct StopDetectStream<S> {
        #[pin]
        inner: S,
        stop: Vec<String>,
        stopped: bool,
        sent_len: usize,
        buffer: String,
        include_obfuscation: bool,
    }
}

impl<S> Stream for StopDetectStream<S>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
{
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(mut chunk))) => {
                    if *this.stopped {
                        if chunk.choices.is_empty() && chunk.usage.is_some() {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        // 允许 finish_reason 从 stop 升级为 tool_calls
                        if let Some(choice) = chunk.choices.first_mut()
                            && choice.delta.content.is_none()
                            && choice.delta.reasoning_content.is_none()
                            && choice.delta.tool_calls.is_none()
                            && choice.finish_reason == Some(FINISH_TOOL_CALLS)
                        {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        continue;
                    }

                    if let Some(choice) = chunk.choices.first_mut()
                        && let Some(ref content) = choice.delta.content
                    {
                        this.buffer.push_str(content);
                        if let Some(pos) = find_stop_pos(this.buffer, this.stop) {
                            trace!(target: "adapter", ">>> stop: truncate at {pos}");
                            // sent_len 可能已经越过 stop 位置（stop 串跨 chunk / 多语言字节边界），
                            // 此时没有新内容可发；同时两端都对齐到 char 边界避免切片 panic。
                            let safe_start =
                                floor_char_boundary(this.buffer, (*this.sent_len).min(pos));
                            let safe_end = floor_char_boundary(this.buffer, pos);
                            let truncated = &this.buffer[safe_start..safe_end];
                            if truncated.is_empty() {
                                choice.delta.content = None;
                            } else {
                                choice.delta.content = Some(truncated.to_string());
                            }
                            choice.finish_reason = Some(FINISH_STOP);
                            *this.stopped = true;
                            this.buffer.clear();
                            *this.sent_len = pos;
                        } else {
                            *this.sent_len = this.buffer.len();
                        }
                    }
                    if *this.include_obfuscation && !chunk.choices.is_empty() {
                        let without = serde_json::to_string(&chunk)
                            .map_err(|e| OpenAIAdapterError::Internal(format!("json: {e}")))?;
                        let overhead = r#","obfuscation":"""#.len();
                        let pad_len = if without.len() + overhead < OBFUSCATION_TARGET_LEN {
                            OBFUSCATION_TARGET_LEN - without.len() - overhead
                        } else {
                            OBFUSCATION_MIN_PAD
                        };
                        chunk.obfuscation = Some(random_padding(pad_len));
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// 流式响应参数（减少 stream() 参数个数）
pub(crate) struct StreamCfg {
    pub include_usage: bool,
    pub include_obfuscation: bool,
    pub stop: Vec<String>,
    pub prompt_tokens: u32,
    pub repair_fn: Option<RepairFn>,
    pub tag_config: Arc<TagConfig>,
}

/// 流式响应：把 StreamEvent 流转换为 ChatCompletionsResponseChunk 流
pub(crate) fn stream<S>(ds_stream: S, model: String, cfg: StreamCfg) -> ChunkStream
where
    S: Stream<Item = Result<StreamEvent, ds_core::CoreError>> + Send + 'static,
{
    debug!(
        target: "adapter",
        "构建流式响应: model={}, include_usage={}, include_obfuscation={}, stop_count={}, repair={}",
        model, cfg.include_usage, cfg.include_obfuscation, cfg.stop.len(), cfg.repair_fn.is_some()
    );
    let converted = converter::ConverterStream::new(
        ds_stream.map(|r| r.map_err(OpenAIAdapterError::from)),
        model.clone(),
        cfg.include_usage,
        cfg.include_obfuscation,
        cfg.prompt_tokens,
    );
    let tool_parsed = tool_parser::ToolCallStream::new(converted, model.clone(), cfg.tag_config);
    let tool_boxed: Pin<
        Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>,
    > = Box::pin(tool_parsed);

    let after_repair: Pin<
        Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>,
    > = if let Some(f) = cfg.repair_fn {
        Box::pin(RepairStream::new(tool_boxed, f, model))
    } else {
        tool_boxed
    };

    let stop_detect = StopDetectStream {
        inner: after_repair,
        stop: cfg.stop,
        stopped: false,
        sent_len: 0,
        buffer: String::new(),
        include_obfuscation: cfg.include_obfuscation,
    };
    Box::pin(stop_detect)
}

/// 非流式响应：stream() 的下游收集器，纯重组无特殊逻辑
///
/// 始终保持为 stream() 的流式收集和重组：
/// - 所有核心处理（修复、转换、停止序列）都在 stream() 中完成
/// - 本函数仅将 stream() 的输出事件聚合并重组成单条 ChatCompletionsResponse JSON
/// - 不要在此函数中添加任何独立于 stream() 的处理逻辑
pub(crate) async fn aggregate<S>(
    ds_stream: S,
    model: String,
    cfg: StreamCfg,
) -> Result<ChatCompletionsResponse, OpenAIAdapterError>
where
    S: Stream<Item = Result<StreamEvent, ds_core::CoreError>> + Send + 'static,
{
    debug!(target: "adapter", "building non-streaming response: model={}, stop_count={}", model, cfg.stop.len());
    let chunk_stream = stream(
        ds_stream,
        model.clone(),
        StreamCfg {
            include_usage: true,
            include_obfuscation: false,
            ..cfg
        },
    );
    futures::pin_mut!(chunk_stream);

    let mut id = String::new();
    let mut created = 0u64;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Option<Vec<ToolCall>> = None;
    let mut usage = None;
    let mut finish_reason: Option<&'static str> = None;

    while let Some(res) = chunk_stream.next().await {
        let chunk = res?;

        if id.is_empty() {
            id = chunk.id;
            created = chunk.created;
        }

        if let Some(u) = chunk.usage {
            usage = Some(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            });
        }

        if let Some(choice) = chunk.choices.into_iter().next() {
            if finish_reason.is_none() {
                finish_reason = choice.finish_reason;
            }
            if let Some(c) = choice.delta.content {
                content.push_str(&c);
            }
            if let Some(r) = choice.delta.reasoning_content {
                reasoning.push_str(&r);
            }
            if let Some(tc) = choice.delta.tool_calls
                && !tc.is_empty()
            {
                tool_calls = Some(tc);
            }
        }
    }

    let reasoning_content = if reasoning.is_empty() {
        None
    } else {
        Some(reasoning)
    };

    let has_tool_calls = tool_calls.is_some();
    let message_content = if content.is_empty() && !has_tool_calls {
        warn!(
            target: "adapter",
            "聚合响应内容为空: model={}, finish_reason={:?}, has_tool_calls={}, usage={:?}",
            model, finish_reason, tool_calls.is_some(), usage
        );
        None
    } else {
        Some(content)
    };
    // OpenAI 的非流式响应总是给出 finish_reason；上游 EOF 未给时应退化为 `stop`
    // 而不是序列化成 `null`，否则部分客户端会把它当成异常中断。
    let final_reason = if has_tool_calls {
        Some(FINISH_TOOL_CALLS)
    } else {
        finish_reason.or(Some(FINISH_STOP))
    };

    let completion = ChatCompletionsResponse {
        id,
        object: "chat.completion",
        created,
        model,
        choices: vec![Choice {
            index: 0,
            message: MessageResponse {
                role: "assistant",
                content: message_content,
                reasoning_content,
                refusal: None,
                annotations: None,
                audio: None,
                function_call: None,
                tool_calls,
            },
            finish_reason: final_reason,
            logprobs: None,
        }],
        usage,
        service_tier: None,
        system_fingerprint: None,
    };

    debug!(
        target: "adapter",
        "非流式响应聚合完成: finish_reason={:?}, has_tool_calls={}, usage={:?}",
        completion.choices[0].finish_reason,
        completion.choices[0].message.tool_calls.is_some(),
        completion.usage
    );
    Ok(completion)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use futures::StreamExt;

    use ds_core::StreamEvent;

    use super::*;

    fn default_tag_config() -> Arc<TagConfig> {
        Arc::new(TagConfig::from_config(
            &crate::config::ToolCallTagConfig::default(),
        ))
    }

    fn tool_span(content: &str) -> String {
        format!(
            "{}{}{}",
            tool_parser::TOOL_CALL_START,
            content,
            tool_parser::TOOL_CALL_END
        )
    }

    fn make_event_stream(
        pieces: &[(&str, &str)],
        usage_tokens: Option<u32>,
    ) -> Vec<Result<StreamEvent, ds_core::CoreError>> {
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut has_think = false;
        let mut has_content = false;

        for (content, frag_type) in pieces {
            if *frag_type == "THINK" {
                if !has_think {
                    events.push(StreamEvent::ThinkStart);
                    has_think = true;
                }
                events.push(StreamEvent::ThinkDelta {
                    content: content.to_string(),
                });
            } else {
                if !has_content {
                    events.push(StreamEvent::ContentStart);
                    has_content = true;
                }
                events.push(StreamEvent::ContentDelta {
                    content: content.to_string(),
                });
            }
        }

        let finish = has_content.then(|| "stop".to_string());
        events.push(StreamEvent::Done {
            finish_reason: finish,
            accumulated_token_usage: usage_tokens,
        });

        events.into_iter().map(Ok).collect()
    }

    fn meta_event() -> Vec<Result<StreamEvent, ds_core::CoreError>> {
        vec![Ok(StreamEvent::Meta {
            account_id: "test".into(),
        })]
    }

    fn make_full_stream(
        pieces: &[(&str, &str)],
        usage_tokens: Option<u32>,
    ) -> Vec<Result<StreamEvent, ds_core::CoreError>> {
        let mut events = meta_event();
        events.extend(make_event_stream(pieces, usage_tokens));
        events
    }

    /// 回归：解析失败时错误消息截取前 200 字节做预览，多语言输出下必须落在
    /// char 边界，否则会 panic（与此前 stop 截断 panic 同源）。
    #[tokio::test]
    async fn execute_tool_repair_preview_does_not_split_multibyte() {
        let text = format!(
            "{}[{{\"name\": \"f\", \"arguments\": {{\"a\": \"{}\"",
            tool_parser::TOOL_CALL_START,
            "中文".repeat(80)
        );
        let events = make_event_stream(&[(text.as_str(), "RESPONSE")], None);
        let cfg = default_tag_config();
        let result = execute_tool_repair(Box::pin(futures::stream::iter(events)), &cfg).await;
        assert!(result.is_err(), "非法 JSON 应返回错误而不是 panic");
    }

    #[tokio::test]
    async fn aggregate_plain_text() {
        let events = make_full_stream(&[("hello world", "RESPONSE")], Some(41));
        let stream = futures::stream::iter(events);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.object, "chat.completion");
        assert_eq!(resp.model, "deepseek-default");
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("hello world"));
        assert_eq!(resp.choices[0].finish_reason, Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().completion_tokens, 41);
    }

    /// 回归（issue #58）：中文逐字流式输出时，聚合结果必须与原文**逐字节一致**，
    /// 不允许出现替换字符或乱码。同时锁住「输出链路不做任何 lossy UTF-8 转换」这一性质。
    #[tokio::test]
    async fn multibyte_content_survives_stream_chain() {
        let text = "只有一个分支 master，目前处于 V1.0.0 RC1 版本。路径：/home/用户/项目";
        let chunks: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let pieces: Vec<(&str, &str)> = chunks.iter().map(|c| (c.as_str(), "RESPONSE")).collect();
        let events = make_full_stream(&pieces, None);
        let stream = futures::stream::iter(events);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let content = resp.choices[0].message.content.clone().unwrap_or_default();
        assert_eq!(content, text, "多字节内容在流式链路中必须逐字节保留");
        assert!(!content.contains('\u{FFFD}'), "不得出现替换字符");
    }

    #[tokio::test]
    async fn aggregate_stop_truncation_multibyte() {
        // 回归：stop 串出现在多字节字符之后，字节位置不落在 char 边界上时不得 panic
        let events = make_full_stream(&[("中文回答完了\n### STOP", "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec!["### STOP".to_string()],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("中文回答完了\n")
        );
        assert_eq!(resp.choices[0].finish_reason, Some("stop"));
    }

    #[tokio::test]
    async fn aggregate_stop_across_chunks() {
        // 回归：stop 串跨 chunk 拼出，其起点落在已发送内容之内（sent_len > pos）。
        // 修复前 `&buffer[sent_len..pos]` 反向切片直接 panic（begin > end）。
        let chunks = vec![Ok(content_chunk("中文abcSTO")), Ok(content_chunk("Pxyz"))];
        let out: Vec<_> = StopDetectStream {
            inner: futures::stream::iter(chunks),
            stop: vec!["STOP".to_string()],
            stopped: false,
            sent_len: 0,
            buffer: String::new(),
            include_obfuscation: false,
        }
        .collect()
        .await;

        let text: String = out
            .into_iter()
            .map(Result::unwrap)
            .filter_map(|c| c.choices.into_iter().next())
            .filter_map(|c| c.delta.content)
            .collect();
        // 第一段已整段发出（当时 stop 尚未出现），第二段补齐 "STOP" 时其起点在已发送内容之中，
        // 因此第二段无新内容可发；关键是不得 panic。
        assert_eq!(text, "中文abcSTO");
    }

    fn content_chunk(content: &str) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: "chatcmpl-test".into(),
            object: "chat.completion.chunk",
            created: 0,
            model: "deepseek-default".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    content: Some(content.to_string()),
                    ..Default::default()
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    #[tokio::test]
    async fn aggregate_thinking() {
        let events = make_full_stream(&[("thinking", "THINK"), ("answer", "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let resp = aggregate(
            stream,
            "deepseek-expert".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(msg.content.as_deref(), Some("answer"));
        assert_eq!(resp.choices[0].finish_reason, Some("stop"));
    }

    #[tokio::test]
    async fn aggregate_tool_calls() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let events = make_full_stream(&[(&tool_xml, "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some(""));
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].ty, "function");
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"beijing"}"#
        );
        assert_eq!(resp.choices[0].finish_reason, Some("tool_calls"));
    }

    use std::pin::Pin;

    fn to_bytes_stream(
        st: ChunkStream,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>> {
        Box::pin(st.map(|r| r.and_then(|c| sse_serialize(&c))))
    }

    async fn collect_chunks(
        st: Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let mut st = st;
        while let Some(res) = st.next().await {
            let text = String::from_utf8(res.unwrap().to_vec()).unwrap();
            let json = text
                .strip_prefix("data: ")
                .unwrap()
                .strip_suffix("\n\n")
                .unwrap();
            out.push(serde_json::from_str(json).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn stream_plain_text() {
        let events = make_full_stream(&[("hi", "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(all_content, "hi");
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[tokio::test]
    async fn stream_include_usage() {
        let events = make_full_stream(&[("x", "RESPONSE")], Some(12));
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: true,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(all_content, "x");
        let usage_chunk = chunks
            .iter()
            .find(|c| c["usage"]["completion_tokens"].as_i64() == Some(12));
        assert!(usage_chunk.is_some(), "should have usage chunk");
        let finish_chunk = chunks.iter().rev().find(|c| {
            c["choices"].as_array().is_some_and(|a| !a.is_empty())
                && c["choices"][0]["finish_reason"].as_str().is_some()
        });
        assert_eq!(finish_chunk.unwrap()["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn stream_tool_calls() {
        let tool_xml = tool_span(r#"[{"name": "f", "arguments": {}}]"#);
        let events = make_full_stream(&[(&tool_xml, "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk");
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            !all_content.contains(tool_parser::TOOL_CALL_START),
            "content should not contain tool_calls tags"
        );
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[tokio::test]
    async fn stream_fragmented_tool_calls_with_thinking() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "北京"}}]"#);
        let events = make_full_stream(&[("思考中", "THINK"), (&tool_xml, "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let all_reasoning: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str())
            .collect();
        assert!(all_reasoning.contains("思考中"), "should contain 思考中");
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk");
        let tc_chunk = chunks
            .iter()
            .find(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some())
            .unwrap();
        let calls = tc_chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"北京"}"#);
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[tokio::test]
    async fn stream_tool_calls_with_leading_text_fragmented() {
        let tool_xml = tool_span(
            r#"[{"name": "astrbot_execute_shell", "arguments": {"command": "cat /data/astrbot/skills/doubao-image-gen/SKILL.md"}}]"#,
        );
        let events = make_full_stream(
            &[
                ("好的，我来帮你用豆包生成图片。", "RESPONSE"),
                (&tool_xml, "RESPONSE"),
            ],
            None,
        );
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            all_content.contains("好的，我来帮你用豆包生成图片"),
            "should contain leading text, got {all_content:?}"
        );
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk");
        let tc_chunk = chunks
            .iter()
            .find(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some())
            .unwrap();
        let calls = tc_chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "astrbot_execute_shell");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_no_leading_text() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let events = make_full_stream(&[(&tool_xml, "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(
            chunks.len() >= 2,
            "expected at least 2 chunks, got {}",
            chunks.len()
        );
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let tc_idx = chunks
            .iter()
            .position(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some())
            .expect("should have a chunk with tool_calls");
        let tc_chunk = &chunks[tc_idx];
        let calls = tc_chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"beijing"}"#);
        let last = chunks.last().unwrap();
        assert_eq!(
            last["choices"][0]["finish_reason"], "tool_calls",
            "finish_reason should be tool_calls, got {:?}",
            last["choices"][0]["finish_reason"]
        );
    }

    /// 回归：tool_calls 场景必须保留 usage。
    ///
    /// 上游在工具调用之后还会送一个带 finish_reason + usage 的收尾 chunk；
    /// 早期实现在 ToolParseState::Done 命中时就立刻发出结束 chunk，
    /// 导致该带 usage 的 chunk 被丢弃，completion_tokens 恒为 0。
    #[tokio::test]
    async fn stream_tool_calls_preserves_usage() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        // 关键：工具调用之后模型还会继续输出一段文字（上游真实行为，
        // 也是 Done 状态要防的幻觉）。该 chunk 会命中 Done 分支，
        // 早期实现正是在这里提前结束流，丢掉了随后带 usage 的收尾 chunk。
        let events = make_full_stream(
            &[
                (&tool_xml, "RESPONSE"),
                (
                    "

以上是查询结果。",
                    "RESPONSE",
                ),
            ],
            Some(811),
        );
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: true,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 887,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;

        // 结束 chunk 必须是 tool_calls，且携带上游累计 usage
        // （与 OpenAI 一致：usage 出现在最后一个 chunk）
        let last = chunks.last().unwrap();
        assert_eq!(
            last["choices"][0]["finish_reason"], "tool_calls",
            "最后一个 chunk 的 finish_reason 应为 tool_calls"
        );
        let usage = last
            .get("usage")
            .filter(|u| !u.is_null())
            .unwrap_or_else(|| panic!("tool_calls 结束 chunk 必须包含 usage, 实际: {last}"));
        assert_eq!(
            usage["completion_tokens"], 811,
            "completion_tokens 应保留上游累计用量, 实际: {usage:?}"
        );
        assert_eq!(usage["prompt_tokens"], 887);
        assert_eq!(usage["total_tokens"], 1698);
    }

    #[tokio::test]
    async fn stream_with_tool_search_and_open() {
        let events = make_full_stream(&[("hello", "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(all_content, "hello");
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[tokio::test]
    async fn stream_include_obfuscation() {
        let events = make_full_stream(
            &[("这是一段足够长的中文文本用于测试混淆", "RESPONSE")],
            None,
        );
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: true,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        assert!(chunks.len() >= 2);
        for c in &chunks {
            if c["choices"][0]["delta"]["content"].as_str().is_some()
                || c["choices"][0]["finish_reason"].as_str().is_some()
            {
                assert!(
                    c["obfuscation"].as_str().is_some(),
                    "obfuscation 必须是 chunk 的顶层字段（OpenAI 规范），实际: {c}"
                );
                let len = serde_json::to_string(c).unwrap().len();
                assert!(
                    (490..=530).contains(&len),
                    "chunk len {len} out of expected 490..=530 range"
                );
            }
        }
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            all_content.contains("足够长的中文文本"),
            "should contain expected text, got {all_content:?}"
        );
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[tokio::test]
    async fn aggregate_tool_calls_with_leading_text() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let events = make_full_stream(
            &[("好的，我来帮你。", "RESPONSE"), (&tool_xml, "RESPONSE")],
            None,
        );
        let stream = futures::stream::iter(events);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("好的，我来帮你。"));
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"beijing"}"#
        );
        assert_eq!(resp.choices[0].finish_reason, Some("tool_calls"));
    }

    #[tokio::test]
    async fn aggregate_tool_calls_multi_chunk_fragments() {
        let tool_xml = tool_span(r#"[{"name": "f", "arguments": {}}]"#);
        let events = make_full_stream(
            &[("让我来查一下。", "RESPONSE"), (&tool_xml, "RESPONSE")],
            None,
        );
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk but didn't");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_with_thinking_then_leading_text_then_fragmented_json() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let events = make_full_stream(
            &[
                ("用户要查天气，我需要调用工具", "THINK"),
                ("好的，我来帮你查一下。", "RESPONSE"),
                (&tool_xml, "RESPONSE"),
            ],
            None,
        );
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk but didn't");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_json_split_right_after_tag() {
        let tool_xml = tool_span(r#"[{"name": "f", "arguments": {}}]"#);
        let events = make_full_stream(&[("好的。", "RESPONSE"), (&tool_xml, "RESPONSE")], None);
        let stream = futures::stream::iter(events);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk but didn't");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }
}

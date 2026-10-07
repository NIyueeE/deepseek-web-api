//! Responses API 响应转换 —— ChatCompletionsResponse/Chunk → Response 对象与流式事件
//!
//! 事件序列严格对照 openai-openapi 的 `ResponseStreamEvent` 定义与
//! openai-python `openai/lib/streaming/responses/` 的累积逻辑：
//!
//! ```text
//! response.created
//! response.in_progress
//! response.output_item.added            (reasoning | message | function_call)
//! response.reasoning_summary_part.added (reasoning 项)
//! response.reasoning_summary_text.delta (reasoning 项)
//! response.content_part.added           (message 项)
//! response.output_text.delta            (message 项)
//! response.function_call_arguments.delta(delta 形态)
//! response.output_text.done
//! response.content_part.done
//! response.reasoning_summary_text.done
//! response.reasoning_summary_part.done
//! response.output_item.done
//! response.completed
//! data: [DONE]
//! ```
//!
//! 关键差异（相对 Chat Completions）：
//! - usage 字段名为 `input_tokens`/`output_tokens`/`total_tokens`，且必须带
//!   `input_tokens_details.cached_tokens` 与 `output_tokens_details.reasoning_tokens`
//! - 每个事件既带 `type`（与 SSE `event:` 同名）也带 `sequence_number`

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use log::{debug, trace, warn};
use pin_project_lite::pin_project;

use crate::openai_adapter::OpenAIAdapterError;
use crate::openai_adapter::types::{ChatCompletionsResponse, ChatCompletionsResponseChunk};

use super::store::StoredTurn;
use super::types::{
    IncompleteDetails, InputTokensDetails, OutputTokensDetails, ResponseError, ResponseObject,
    ResponseUsage, SseEvent, sse_bytes,
};

static RESPONSE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
static ITEM_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
static CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 生成 `resp_<32 hex>` 形式的响应 ID
pub(crate) fn next_response_id() -> String {
    let n = RESPONSE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "resp_{:016x}{:016x}",
        u64::try_from(now).unwrap_or(u64::MAX),
        n
    )
}

fn next_item_id(prefix: &str) -> String {
    let n = ITEM_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{n:016x}")
}

fn next_call_id() -> String {
    let n = CALL_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("call_{n:016x}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 构建 Response 对象所需的上下文（回显请求参数）
#[derive(Debug, Clone)]
pub struct ResponseCtx {
    pub id: String,
    pub model: String,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub tools: Vec<serde_json::Value>,
    pub tool_choice: serde_json::Value,
    pub parallel_tool_calls: bool,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub store: bool,
    pub previous_response_id: Option<String>,
    pub metadata: serde_json::Value,
    pub user: Option<String>,
}

/// 流式响应收尾钩子：拿到最终 Response 对象快照时调用
///
/// 用于写入 `previous_response_id` 缓存与 `GET /v1/responses/{id}` 的检索快照。
pub type FinishHook = std::sync::Arc<dyn Fn(serde_json::Value) + Send + Sync>;

impl ResponseCtx {
    /// 生成进行中的 Response 骨架（`output` 为空）
    fn skeleton(&self, status: &'static str) -> ResponseObject {
        ResponseObject {
            id: self.id.clone(),
            object: "response",
            created_at: now_secs(),
            completed_at: None,
            status,
            error: None,
            incomplete_details: None,
            instructions: self.instructions.clone(),
            max_output_tokens: self.max_output_tokens,
            model: self.model.clone(),
            output: Vec::new(),
            output_text: None,
            parallel_tool_calls: self.parallel_tool_calls,
            previous_response_id: self.previous_response_id.clone(),
            reasoning: super::types::ReasoningSummary {
                effort: self.reasoning_effort.clone(),
                summary: self.reasoning_summary.clone(),
            },
            store: self.store,
            temperature: self.temperature,
            text: super::types::TextSummary {
                format: serde_json::json!({"type": "text"}),
            },
            tool_choice: self.tool_choice.clone(),
            tools: self.tools.clone(),
            top_p: self.top_p,
            truncation: "disabled".to_string(),
            usage: None,
            user: self.user.clone(),
            metadata: self.metadata.clone(),
            service_tier: None,
        }
    }
}

/// 把 Chat Completions 响应对象转换为 Response 对象（非流式）
#[must_use]
pub fn from_chat_completions(resp: &ChatCompletionsResponse, ctx: &ResponseCtx) -> ResponseObject {
    let mut output: Vec<serde_json::Value> = Vec::new();
    let mut text_accum = String::new();
    let mut reasoning_accum = String::new();

    if let Some(choice) = resp.choices.first() {
        let msg = &choice.message;

        if let Some(thinking) = msg.reasoning_content.as_ref().filter(|t| !t.is_empty()) {
            reasoning_accum.push_str(thinking);
            output.push(reasoning_item(&next_item_id("rs"), thinking, "completed"));
        }

        if let Some(content) = msg.content.as_ref().filter(|t| !t.is_empty()) {
            text_accum.push_str(content);
        }

        if let Some(calls) = msg.tool_calls.as_ref() {
            for call in calls {
                let call_id = if call.id.is_empty() {
                    next_call_id()
                } else {
                    call.id.clone()
                };
                let (name, arguments) = split_call(call);
                output.push(serde_json::json!({
                    "type": "function_call",
                    "id": next_item_id("fc"),
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                }));
            }
        }

        // 与 OpenAI 一致：有文本就一定产出 message 项（即使同时存在 tool_calls）
        if !text_accum.is_empty() || msg.tool_calls.is_none() {
            output.push(message_item(&next_item_id("msg"), &text_accum, "completed"));
        }
    }

    let finish_reason = resp.choices.first().and_then(|c| c.finish_reason);
    let (status, incomplete_details) = resolve_status(finish_reason);

    let usage = resp.usage.as_ref().map(|u| ResponseUsage {
        input_tokens: u.prompt_tokens,
        input_tokens_details: InputTokensDetails { cached_tokens: 0 },
        output_tokens: u.completion_tokens,
        output_tokens_details: OutputTokensDetails {
            reasoning_tokens: estimate_tokens(&reasoning_accum),
        },
        total_tokens: u.total_tokens,
    });

    let mut obj = ctx.skeleton(status);
    obj.output = output;
    obj.output_text = (!text_accum.is_empty()).then_some(text_accum);
    obj.usage = usage;
    obj.incomplete_details = incomplete_details;
    if status == "completed" {
        obj.completed_at = Some(now_secs());
    }
    obj
}

/// 由 finish_reason 推导响应的 `status`
fn resolve_status(finish_reason: Option<&str>) -> (&'static str, Option<IncompleteDetails>) {
    match finish_reason {
        Some("length") => (
            "incomplete",
            Some(IncompleteDetails {
                reason: "max_output_tokens",
            }),
        ),
        Some("content_filter") => (
            "incomplete",
            Some(IncompleteDetails {
                reason: "content_filter",
            }),
        ),
        _ => ("completed", None),
    }
}

/// 粗略估算 token 数（无 BPE 时的退路，用于 reasoning_tokens 兜底）
fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.chars().count() / 4).unwrap_or(u32::MAX)
}

/// 构造 message 输出项
fn message_item(id: &str, text: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "id": id,
        "status": status,
        "role": "assistant",
        "content": [output_text_part(text, &[])],
    })
}

/// 构造 `output_text` 内容块
fn output_text_part(text: &str, annotations: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({
        "type": "output_text",
        "text": text,
        "annotations": annotations,
        "logprobs": [],
    })
}

/// 构造 reasoning 输出项（Responses API 使用 `summary` 数组承载推理摘要）
fn reasoning_item(id: &str, text: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "reasoning",
        "id": id,
        "summary": [{
            "type": "summary_text",
            "text": text,
        }],
        "status": status,
    })
}

fn split_call(call: &crate::openai_adapter::types::ToolCall) -> (String, String) {
    call.function.as_ref().map_or_else(
        || {
            call.custom.as_ref().map_or_else(
                || (String::new(), "{}".to_string()),
                |c| {
                    (
                        c.name.clone(),
                        c.input
                            .as_ref()
                            .map_or_else(|| "{}".to_string(), |v| v.to_string()),
                    )
                },
            )
        },
        |f| (f.name.clone(), f.arguments.clone()),
    )
}

/// 流式状态机
struct StreamState {
    ctx: ResponseCtx,
    sequence: u64,
    created: bool,
    finished: bool,
    /// 已产出的 output 项（最终进入 response.completed）
    output: Vec<serde_json::Value>,
    /// 累积的文本，用于 output_text 与 message 项收尾
    text: String,
    /// 累积的推理文本
    reasoning: String,
    /// 当前 message 项 id / output_index
    message_item: Option<(String, usize)>,
    /// 当前 reasoning 项 id / output_index
    reasoning_item: Option<(String, usize)>,
    /// 已发出的 tool_call 序号
    tool_calls: usize,
    /// 上游累计 usage
    usage: Option<ResponseUsage>,
    /// 结束原因
    finish_reason: Option<String>,
    /// 待发事件队列
    pending: Vec<SseEvent>,
    /// 收尾钩子（仅在成功完成时调用）
    on_finish: Option<FinishHook>,
    /// 已拿到 finish_reason 但仍在等上游的最终 usage 块
    awaiting_usage: bool,
}

impl StreamState {
    fn new(ctx: ResponseCtx, on_finish: Option<FinishHook>) -> Self {
        Self {
            ctx,
            sequence: 0,
            created: false,
            finished: false,
            output: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
            message_item: None,
            reasoning_item: None,
            tool_calls: 0,
            usage: None,
            finish_reason: None,
            pending: Vec::new(),
            on_finish,
            awaiting_usage: false,
        }
    }

    fn emit(&mut self, name: &'static str, mut data: serde_json::Value) {
        let seq = self.sequence;
        self.sequence += 1;
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "type".to_string(),
                serde_json::Value::String(name.to_string()),
            );
            obj.insert("sequence_number".to_string(), serde_json::Value::from(seq));
        }
        self.pending.push((name, data));
    }

    fn emit_created(&mut self) {
        if self.created {
            return;
        }
        self.created = true;
        let snapshot = self.ctx.skeleton("in_progress");
        self.emit(
            "response.created",
            serde_json::json!({ "response": snapshot }),
        );
        let snapshot = self.ctx.skeleton("in_progress");
        self.emit(
            "response.in_progress",
            serde_json::json!({ "response": snapshot }),
        );
    }

    /// 保证 message 项已创建，返回 (item_id, output_index)
    fn ensure_message_item(&mut self) -> (String, usize) {
        if let Some(existing) = &self.message_item {
            return existing.clone();
        }
        let id = next_item_id("msg");
        let index = self.output.len();
        self.output.push(message_item(&id, "", "in_progress"));
        self.emit(
            "response.output_item.added",
            serde_json::json!({
                "output_index": index,
                "item": {
                    "type": "message",
                    "id": id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        );
        self.emit(
            "response.content_part.added",
            serde_json::json!({
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "part": output_text_part("", &[]),
            }),
        );
        self.message_item = Some((id.clone(), index));
        (id, index)
    }

    /// 保证 reasoning 项已创建，返回 (item_id, output_index)
    fn ensure_reasoning_item(&mut self) -> (String, usize) {
        if let Some(existing) = &self.reasoning_item {
            return existing.clone();
        }
        let id = next_item_id("rs");
        let index = self.output.len();
        self.output.push(reasoning_item(&id, "", "in_progress"));
        self.emit(
            "response.output_item.added",
            serde_json::json!({
                "output_index": index,
                "item": {
                    "type": "reasoning",
                    "id": id,
                    "summary": [],
                },
            }),
        );
        self.emit(
            "response.reasoning_summary_part.added",
            serde_json::json!({
                "item_id": id,
                "output_index": index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": ""},
            }),
        );
        self.reasoning_item = Some((id.clone(), index));
        (id, index)
    }

    /// 以完整项形态落库（收尾时用最终文本替换占位项）
    fn finalize_output_items(&mut self) {
        if let Some((id, index)) = self.reasoning_item.clone() {
            let item = reasoning_item(&id, &self.reasoning, "completed");
            if let Some(slot) = self.output.get_mut(index) {
                *slot = item.clone();
            }
            self.emit(
                "response.reasoning_summary_text.done",
                serde_json::json!({
                    "item_id": id,
                    "output_index": index,
                    "summary_index": 0,
                    "text": self.reasoning,
                }),
            );
            self.emit(
                "response.reasoning_summary_part.done",
                serde_json::json!({
                    "item_id": id,
                    "output_index": index,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": self.reasoning},
                }),
            );
            self.emit(
                "response.output_item.done",
                serde_json::json!({ "output_index": index, "item": item }),
            );
        }

        if let Some((id, index)) = self.message_item.clone() {
            let item = message_item(&id, &self.text, "completed");
            if let Some(slot) = self.output.get_mut(index) {
                *slot = item.clone();
            }
            self.emit(
                "response.output_text.done",
                serde_json::json!({
                    "item_id": id,
                    "output_index": index,
                    "content_index": 0,
                    "text": self.text,
                    "logprobs": [],
                }),
            );
            self.emit(
                "response.content_part.done",
                serde_json::json!({
                    "item_id": id,
                    "output_index": index,
                    "content_index": 0,
                    "part": output_text_part(&self.text, &[]),
                }),
            );
            self.emit(
                "response.output_item.done",
                serde_json::json!({ "output_index": index, "item": item }),
            );
        }
    }

    /// 处理一个上游 chunk，事件累积到 `pending` 由 poll 侧统一冲刷
    fn handle_chunk(&mut self, chunk: ChatCompletionsResponseChunk) {
        self.emit_created();

        if let Some(u) = &chunk.usage {
            self.usage = Some(ResponseUsage {
                input_tokens: u.prompt_tokens,
                input_tokens_details: InputTokensDetails { cached_tokens: 0 },
                output_tokens: u.completion_tokens,
                output_tokens_details: OutputTokensDetails {
                    reasoning_tokens: estimate_tokens(&self.reasoning),
                },
                total_tokens: u.total_tokens,
            });
            // 上游把 usage 单独放在尾块里：此刻才具备发出收尾事件的条件
            if self.awaiting_usage {
                self.awaiting_usage = false;
                self.emit_completed();
            }
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };

        let text_delta = choice.delta.content.unwrap_or_default();
        let reasoning_delta = choice.delta.reasoning_content.unwrap_or_default();

        if !reasoning_delta.is_empty() {
            let (id, index) = self.ensure_reasoning_item();
            self.reasoning.push_str(&reasoning_delta);
            self.emit(
                "response.reasoning_summary_text.delta",
                serde_json::json!({
                    "item_id": id,
                    "output_index": index,
                    "summary_index": 0,
                    "delta": reasoning_delta,
                }),
            );
        }

        if !text_delta.is_empty() {
            let (id, index) = self.ensure_message_item();
            self.text.push_str(&text_delta);
            self.emit(
                "response.output_text.delta",
                serde_json::json!({
                    "item_id": id,
                    "output_index": index,
                    "content_index": 0,
                    "delta": text_delta,
                    "logprobs": [],
                }),
            );
        }

        if let Some(calls) = choice.delta.tool_calls
            && !calls.is_empty()
        {
            for call in calls {
                let (name, arguments) = split_call(&call);
                let call_id = if call.id.is_empty() {
                    next_call_id()
                } else {
                    call.id.clone()
                };
                let id = next_item_id("fc");
                let index = self.output.len();
                self.output.push(serde_json::json!({
                    "type": "function_call",
                    "id": id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                }));
                self.tool_calls += 1;
                self.emit(
                    "response.output_item.added",
                    serde_json::json!({
                        "output_index": index,
                        "item": {
                            "type": "function_call",
                            "id": id,
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress",
                        },
                    }),
                );
                self.emit(
                    "response.function_call_arguments.delta",
                    serde_json::json!({
                        "item_id": id,
                        "output_index": index,
                        "delta": arguments,
                    }),
                );
                self.emit(
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "item_id": id,
                        "output_index": index,
                        "arguments": arguments,
                    }),
                );
                self.emit(
                    "response.output_item.done",
                    serde_json::json!({
                        "output_index": index,
                        "item": self.output[index].clone(),
                    }),
                );
            }
        }

        if let Some(reason) = choice.finish_reason {
            self.finish_reason = Some(reason.to_string());
            self.finalize_output_items();
            if self.usage.is_some() {
                self.emit_completed();
            } else {
                // Responses 的 usage 挂在 `response.completed` 事件体内，
                // 上游可能在其后再送一个纯 usage 块（Chat Completions 语义）。
                // 此时先不发收尾事件，等 usage 到达或上游 EOF 再发，
                // 避免客户端拿到一个 usage 为 null 的终态。
                self.awaiting_usage = true;
            }
        }
    }

    fn emit_completed(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;

        let finish_reason = self.finish_reason.clone();
        let (status, incomplete_details) = resolve_status(finish_reason.as_deref());

        let mut snapshot = self.ctx.skeleton(status);
        snapshot.output.clone_from(&self.output);
        snapshot.output_text = (!self.text.is_empty()).then(|| self.text.clone());
        snapshot.usage.clone_from(&self.usage);
        snapshot.incomplete_details = incomplete_details;
        if status == "completed" {
            snapshot.completed_at = Some(now_secs());
        }

        // 落库（供后续 previous_response_id 与 GET /v1/responses/{id} 使用）
        if let Some(hook) = self.on_finish.take()
            && let Ok(snapshot_json) = serde_json::to_value(&snapshot)
        {
            hook(snapshot_json);
        }

        let name = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.emit(name, serde_json::json!({ "response": snapshot }));
    }

    /// 上游异常：产出 `response.failed`
    fn emit_failed(&mut self, message: &str) {
        self.emit_created();
        self.finished = true;
        let mut snapshot = self.ctx.skeleton("failed");
        snapshot.output.clone_from(&self.output);
        snapshot.error = Some(ResponseError {
            code: "server_error".to_string(),
            message: message.to_string(),
        });
        self.emit(
            "response.failed",
            serde_json::json!({ "response": snapshot }),
        );
    }

    /// 流正常结束但未收到 finish_reason（上游断流）
    fn emit_on_eof(&mut self) {
        if self.finished {
            return;
        }
        if self.message_item.is_some() || self.reasoning_item.is_some() || self.tool_calls > 0 {
            debug!(target: "responses_adapter", "upstream EOF without finish_reason, closing as completed");
        }
        self.finalize_output_items();
        self.emit_completed();
    }
}

pin_project! {
    /// ChatCompletionsResponseChunk 流 → Responses SSE 事件流
    struct ResponsesStream<S> {
        #[pin]
        inner: S,
        state: StreamState,
        // 上游流已结束（正常 EOF 或错误），不再轮询
        inner_done: bool,
        // 已发送终止的 `data: [DONE]`
        sent_done: bool,
    }
}

impl<S> Stream for ResponsesStream<S>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
{
    type Item = Result<Bytes, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            // 1) 先冲刷待发事件，保证事件顺序与 sequence_number 一致
            if let Some(event) = this.state.pending.first().cloned() {
                this.state.pending.remove(0);
                return Poll::Ready(Some(Ok(sse_bytes(&event))));
            }

            // 2) 所有事件发完后补 `data: [DONE]`
            if *this.inner_done {
                if *this.sent_done {
                    return Poll::Ready(None);
                }
                *this.sent_done = true;
                return Poll::Ready(Some(Ok(Bytes::from_static(b"data: [DONE]\n\n"))));
            }

            // 3) 拉取上游
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    trace!(target: "responses_adapter", "<<< chunk id={}", chunk.id);
                    this.state.handle_chunk(chunk);
                }
                Poll::Ready(Some(Err(e))) => {
                    // 已开始响应时不再把错误抛给 HTTP 层（SSE 已 200），
                    // 改用协议内的 `response.failed` 事件收尾。
                    warn!(target: "responses_adapter", "upstream stream error: {e}");
                    this.state.emit_failed(&e.to_string());
                    *this.inner_done = true;
                }
                Poll::Ready(None) => {
                    this.state.emit_on_eof();
                    *this.inner_done = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// 将 Chat Completions chunk 流转换为 Responses SSE 字节流
pub(crate) fn stream<S>(
    chunk_stream: S,
    ctx: ResponseCtx,
    on_finish: Option<FinishHook>,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send + 'static,
{
    debug!(target: "responses_adapter", "building Responses stream: model={}", ctx.model);
    Box::pin(ResponsesStream {
        inner: chunk_stream,
        state: StreamState::new(ctx, on_finish),
        inner_done: false,
        sent_done: false,
    })
}

/// 从已存历史中还原 Chat Completions 消息（供 `previous_response_id` 使用）
#[must_use]
pub fn history_messages(turn: &StoredTurn) -> Vec<crate::openai_adapter::types::Message> {
    use crate::openai_adapter::types::{FunctionCall, Message, MessageContent, ToolCall};

    let mut out = Vec::new();

    // 上一轮的用户输入放在最前
    if let Some(user_text) = &turn.input_text
        && !user_text.is_empty()
    {
        out.push(Message {
            role: "user".to_string(),
            content: Some(MessageContent::Text(user_text.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            function_call: None,
            audio: None,
            refusal: None,
        });
    }

    for item in &turn.output {
        let ty = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match ty {
            "message" => {
                let text = item
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                // 模型只输出工具调用时不会产生文本，跳过空 assistant 消息
                if text.is_empty() {
                    continue;
                }
                out.push(Message {
                    role: "assistant".to_string(),
                    content: Some(MessageContent::Text(text)),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    function_call: None,
                    audio: None,
                    refusal: None,
                });
            }
            "function_call" => {
                let call = ToolCall {
                    id: item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    ty: "function".to_string(),
                    function: Some(FunctionCall {
                        name: item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string(),
                    }),
                    custom: None,
                    index: 0,
                };
                match out.last_mut() {
                    Some(last) if last.role == "assistant" && last.tool_calls.is_some() => {
                        last.tool_calls.as_mut().unwrap().push(call);
                    }
                    _ => out.push(Message {
                        role: "assistant".to_string(),
                        content: None,
                        name: None,
                        tool_call_id: None,
                        tool_calls: Some(vec![call]),
                        function_call: None,
                        audio: None,
                        refusal: None,
                    }),
                }
            }
            // reasoning 项仅用于 OpenAI 侧的加密上下文，不重放到 prompt
            _ => {}
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use crate::openai_adapter::types::{
        ChatCompletionsResponse, ChatCompletionsResponseChunk, Choice, ChunkChoice, Delta,
        FunctionCall, MessageResponse, ToolCall, Usage,
    };

    use super::*;

    fn ctx() -> ResponseCtx {
        ResponseCtx {
            id: "resp_test".to_string(),
            model: "deepseek-default".to_string(),
            instructions: Some("be nice".to_string()),
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            tools: vec![],
            tool_choice: serde_json::json!("auto"),
            parallel_tool_calls: true,
            reasoning_effort: None,
            reasoning_summary: None,
            store: false,
            previous_response_id: None,
            metadata: serde_json::json!({}),
            user: None,
        }
    }

    fn chunk(
        delta: Delta,
        finish: Option<&'static str>,
        usage: Option<Usage>,
    ) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk",
            created: 1,
            model: "deepseek-default".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: finish,
                logprobs: None,
            }],
            usage,
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    /// usage-only chunk（choices 为空），与 `stream_options.include_usage` 的收尾块一致
    fn usage_only_chunk(usage: Usage) -> ChatCompletionsResponseChunk {
        ChatCompletionsResponseChunk {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk",
            created: 1,
            model: "deepseek-default".to_string(),
            choices: vec![],
            usage: Some(usage),
            service_tier: None,
            system_fingerprint: None,
            obfuscation: None,
        }
    }

    fn usage(p: u32, c: u32) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: p + c,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        }
    }

    async fn collect_events(chunks: Vec<ChatCompletionsResponseChunk>) -> Vec<serde_json::Value> {
        let st = futures::stream::iter(chunks.into_iter().map(Ok::<_, OpenAIAdapterError>));
        let mut out = Vec::new();
        let mut stream = super::stream(st, ctx(), None);
        while let Some(item) = stream.next().await {
            let bytes = item.unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            if text.trim() == "data: [DONE]" {
                out.push(serde_json::json!({"type": "__done__"}));
                continue;
            }
            let data = text.split_once("data: ").map(|(_, d)| d.trim()).unwrap();
            out.push(serde_json::from_str(data).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn text_stream_event_order() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                Some(usage(3, 0)),
            ),
            chunk(
                Delta {
                    content: Some("Hello".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("stop"), Some(usage(3, 2))),
        ])
        .await;

        let names: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
                "__done__",
            ]
        );
    }

    #[tokio::test]
    async fn sequence_numbers_are_monotonic_and_present() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    content: Some("hi".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("stop"), Some(usage(1, 1))),
        ])
        .await;

        let mut last = None;
        for e in events.iter().filter(|e| e["type"] != "__done__") {
            let seq = e["sequence_number"]
                .as_u64()
                .expect("sequence_number present");
            if let Some(prev) = last {
                assert!(seq > prev, "sequence_number must increase: {prev} -> {seq}");
            }
            last = Some(seq);
        }
    }

    #[tokio::test]
    async fn completed_carries_usage_and_output() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    content: Some("answer".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("stop"), Some(usage(11, 22))),
        ])
        .await;

        let completed = events
            .iter()
            .find(|e| e["type"] == "response.completed")
            .expect("must emit response.completed");
        let resp = &completed["response"];
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["usage"]["input_tokens"], 11);
        assert_eq!(resp["usage"]["output_tokens"], 22);
        assert_eq!(resp["usage"]["total_tokens"], 33);
        assert_eq!(resp["usage"]["input_tokens_details"]["cached_tokens"], 0);
        assert_eq!(
            resp["usage"]["output_tokens_details"]["reasoning_tokens"],
            0
        );
        assert_eq!(resp["output"][0]["type"], "message");
        assert_eq!(resp["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(resp["output"][0]["content"][0]["text"], "answer");
        assert_eq!(resp["output_text"], "answer");
        assert!(resp["completed_at"].as_u64().is_some());
    }

    #[tokio::test]
    async fn usage_only_trailing_chunk_does_not_break_stream() {
        // 上游在 finish 之后还会送一个 choices 为空的纯 usage 块
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    content: Some("done".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("stop"), None),
            usage_only_chunk(usage(9, 4)),
        ])
        .await;

        let names: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        // 收尾事件只出现一次
        assert_eq!(
            names.iter().filter(|n| **n == "response.completed").count(),
            1
        );
        // 最后一个 response.* 事件必须携带最终 usage
        let completed = events
            .iter()
            .rev()
            .find(|e| e["type"] == "response.completed")
            .unwrap();
        assert_eq!(completed["response"]["usage"]["output_tokens"], 4);
    }

    #[tokio::test]
    async fn tool_call_stream_emits_function_call_events() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    tool_calls: Some(vec![ToolCall {
                        id: "call_abc".to_string(),
                        ty: "function".to_string(),
                        function: Some(FunctionCall {
                            name: "get_weather".to_string(),
                            arguments: r#"{"city":"bj"}"#.to_string(),
                        }),
                        custom: None,
                        index: 0,
                    }]),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("tool_calls"), Some(usage(5, 6))),
        ])
        .await;

        let names: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert!(names.contains(&"response.function_call_arguments.delta"));
        assert!(names.contains(&"response.function_call_arguments.done"));

        let completed = events
            .iter()
            .find(|e| e["type"] == "response.completed")
            .unwrap();
        let item = &completed["response"]["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call_abc");
        assert_eq!(item["name"], "get_weather");
        assert_eq!(item["arguments"], r#"{"city":"bj"}"#);
        assert_eq!(item["status"], "completed");
    }

    #[tokio::test]
    async fn reasoning_stream_emits_reasoning_item() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    reasoning_content: Some("thinking".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    content: Some("answer".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("stop"), Some(usage(1, 1))),
        ])
        .await;

        let names: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert!(names.contains(&"response.reasoning_summary_text.delta"));
        assert!(names.contains(&"response.reasoning_summary_part.added"));

        let completed = events
            .iter()
            .find(|e| e["type"] == "response.completed")
            .unwrap();
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "thinking");
        assert_eq!(output[1]["type"], "message");
        // output_index 必须与数组下标一致
        let idx = events
            .iter()
            .find(|e| e["type"] == "response.reasoning_summary_text.delta")
            .unwrap()["output_index"]
            .as_u64()
            .unwrap();
        assert_eq!(idx, 0);
    }

    #[tokio::test]
    async fn upstream_error_after_start_produces_failed_event() {
        let st = futures::stream::iter(vec![
            Ok(chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            )),
            Ok(chunk(
                Delta {
                    content: Some("partial".to_string()),
                    ..Default::default()
                },
                None,
                None,
            )),
            Err(OpenAIAdapterError::Internal("boom".into())),
        ]);
        let mut stream = super::stream(st, ctx(), None);
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            let text = String::from_utf8(item.unwrap().to_vec()).unwrap();
            if text.trim() == "data: [DONE]" {
                continue;
            }
            let data = text.split_once("data: ").map(|(_, d)| d.trim()).unwrap();
            events.push(serde_json::from_str::<serde_json::Value>(data).unwrap());
        }
        let failed = events
            .iter()
            .find(|e| e["type"] == "response.failed")
            .expect("should emit response.failed");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["error"]["code"], "server_error");
    }

    #[tokio::test]
    async fn eof_without_finish_reason_still_completes() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    content: Some("partial".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
        ])
        .await;
        assert_eq!(
            events.last().unwrap()["type"],
            "__done__",
            "stream must terminate with [DONE]"
        );
        assert!(events.iter().any(|e| e["type"] == "response.completed"));
    }

    #[tokio::test]
    async fn length_finish_reason_maps_to_incomplete() {
        let events = collect_events(vec![
            chunk(
                Delta {
                    role: Some("assistant"),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(
                Delta {
                    content: Some("truncated".to_string()),
                    ..Default::default()
                },
                None,
                None,
            ),
            chunk(Delta::default(), Some("length"), Some(usage(1, 9))),
        ])
        .await;
        let ev = events
            .iter()
            .find(|e| e["type"] == "response.incomplete")
            .expect("should emit response.incomplete");
        assert_eq!(ev["response"]["status"], "incomplete");
        assert_eq!(
            ev["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn non_streaming_response_maps_text_and_usage() {
        let resp = ChatCompletionsResponse {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion",
            created: 1,
            model: "deepseek-default".to_string(),
            choices: vec![Choice {
                index: 0,
                message: MessageResponse {
                    role: "assistant",
                    content: Some("hi".to_string()),
                    reasoning_content: Some("because".to_string()),
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop"),
                logprobs: None,
            }],
            usage: Some(usage(7, 3)),
            service_tier: None,
            system_fingerprint: None,
        };
        let obj = from_chat_completions(&resp, &ctx());
        assert_eq!(obj.object, "response");
        assert_eq!(obj.status, "completed");
        assert_eq!(obj.output.len(), 2);
        assert_eq!(obj.output[0]["type"], "reasoning");
        assert_eq!(obj.output[1]["type"], "message");
        assert_eq!(obj.output_text.as_deref(), Some("hi"));
        let u = obj.usage.unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.total_tokens, 10);
    }

    #[test]
    fn non_streaming_tool_call_maps_to_function_call_item() {
        let resp = ChatCompletionsResponse {
            id: "chatcmpl-2".to_string(),
            object: "chat.completion",
            created: 1,
            model: "m".to_string(),
            choices: vec![Choice {
                index: 0,
                message: MessageResponse {
                    role: "assistant",
                    content: None,
                    reasoning_content: None,
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        ty: "function".to_string(),
                        function: Some(FunctionCall {
                            name: "f".to_string(),
                            arguments: "{}".to_string(),
                        }),
                        custom: None,
                        index: 0,
                    }]),
                },
                finish_reason: Some("tool_calls"),
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
        };
        let obj = from_chat_completions(&resp, &ctx());
        assert_eq!(obj.output.len(), 1);
        assert_eq!(obj.output[0]["type"], "function_call");
        assert_eq!(obj.output[0]["call_id"], "call_1");
        assert!(obj.output_text.is_none());
    }

    #[test]
    fn length_finish_maps_to_incomplete_non_streaming() {
        let resp = ChatCompletionsResponse {
            id: "chatcmpl-3".to_string(),
            object: "chat.completion",
            created: 1,
            model: "m".to_string(),
            choices: vec![Choice {
                index: 0,
                message: MessageResponse {
                    role: "assistant",
                    content: Some("x".to_string()),
                    reasoning_content: None,
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: None,
                },
                finish_reason: Some("length"),
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
        };
        let obj = from_chat_completions(&resp, &ctx());
        assert_eq!(obj.status, "incomplete");
        assert_eq!(obj.incomplete_details.unwrap().reason, "max_output_tokens");
    }
}

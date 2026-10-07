//! OpenAI Responses API 适配层 —— `POST /v1/responses`
//!
//! 职责：在 `openai_adapter` 之上做协议翻译，不直接访问 `ds_core`。
//!
//! 与 Chat Completions 适配层的关系：
//!
//! ```text
//! ResponsesRequest ──request.rs──▶ ChatCompletionsRequest
//!                                        │
//!                                 OpenAIAdapter::chat_completions()
//!                                        │
//!             ChatOutput::Stream ──response::stream──▶ Responses SSE 事件
//!             ChatOutput::Json   ──response::from_chat_completions──▶ Response 对象
//! ```
//!
//! `previous_response_id` 由本模块的进程内缓存（`store.rs`）支撑，
//! 缓存上限/TTL 由 `ResponsesAdapter::new()` 的入参决定
//! （来自 `[ds_core]` 的 `responses_store_capacity` / `responses_store_ttl_secs`）。

mod request;
mod response;
mod store;
pub(crate) mod types;

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use log::debug;

use crate::openai_adapter::{ChatOutput, ChatResult, OpenAIAdapter, OpenAIAdapterError};

pub use response::ResponseCtx;
pub use store::{ResponseStore, StoredTurn};
pub use types::ResponsesRequest;

/// Responses API 流式响应类型（SSE 字节流）
pub type ResponsesStream = Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>;

/// Responses 统一输出
///
/// `Json` 用 `Box` 包裹：`ResponseObject` 字段多、体积远大于流句柄，
/// 不装箱会让整个枚举按最大变体分配。
pub enum ResponsesOutput {
    Stream(ResponsesStream),
    Json(Box<types::ResponseObject>),
}

/// Responses 适配器
pub struct ResponsesAdapter {
    openai_adapter: Arc<OpenAIAdapter>,
    store: ResponseStore,
}

impl ResponsesAdapter {
    /// 创建适配器
    #[must_use]
    pub fn new(
        openai_adapter: Arc<OpenAIAdapter>,
        store_capacity: usize,
        store_ttl_secs: u64,
    ) -> Self {
        Self {
            openai_adapter,
            store: ResponseStore::new(store_capacity, store_ttl_secs),
        }
    }

    /// POST /v1/responses
    ///
    /// 流程：解析 `previous_response_id` → 映射为 ChatCompletionsRequest →
    /// 委托 OpenAIAdapter → 按 stream 分流为 Response 对象或 SSE 事件流。
    pub async fn create(
        &self,
        req: ResponsesRequest,
        request_id: &str,
    ) -> Result<ChatResult<ResponsesOutput>, OpenAIAdapterError> {
        debug!(
            target: "responses_adapter",
            "req={} 收到 responses 请求: model={}, stream={}", request_id, req.model, req.stream
        );

        // 1) 解析历史（previous_response_id）
        let mut history = Vec::new();
        if let Some(prev) = req.previous_response_id.as_deref() {
            let Some(turn) = self.store.get(prev) else {
                return Err(OpenAIAdapterError::BadRequest(format!(
                    "previous_response_id '{prev}' 不存在或已过期；请重放完整 input 后重试"
                )));
            };
            history = response::history_messages(&turn);
            debug!(
                target: "responses_adapter",
                "req={} 从 previous_response_id={} 恢复 {} 条历史消息",
                request_id, prev, history.len()
            );
        }

        // 2) 回显参数与上下文（需在消费 req 之前提取）
        let ctx = Self::build_ctx(&req);
        let input_text = extract_input_text(&req);
        let chat_req = request::into_chat_completions(&req, history)
            .map_err(OpenAIAdapterError::BadRequest)?;

        // 3) 委托 OpenAI 适配器
        let result = self
            .openai_adapter
            .chat_completions(chat_req, request_id)
            .await?;

        let account_id = result.account_id;
        let prompt_tokens = result.prompt_tokens;

        // 4) 按 stream 分流
        let data = match result.data {
            ChatOutput::Stream(stream) => {
                // 流式场景的最终 output 只有消费完整条流才完整，
                // 因此通过收尾钩子在流结束时落库。
                let hook: Option<response::FinishHook> = ctx.store.then(|| {
                    let store = self.store.clone();
                    let id = ctx.id.clone();
                    let input_text = input_text.clone();
                    let hook: response::FinishHook =
                        Arc::new(move |snapshot: serde_json::Value| {
                            let output = snapshot
                                .get("output")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            store.insert(
                                id.clone(),
                                StoredTurn {
                                    input_text: input_text.clone(),
                                    output,
                                    response: snapshot,
                                },
                            );
                        });
                    hook
                });
                ResponsesOutput::Stream(response::stream(stream, ctx, hook))
            }
            ChatOutput::Json(json) => {
                let obj = response::from_chat_completions(&json, &ctx);
                if ctx.store {
                    let snapshot = serde_json::to_value(&obj).unwrap_or(serde_json::Value::Null);
                    self.store.insert(
                        obj.id.clone(),
                        StoredTurn {
                            input_text,
                            output: obj.output.clone(),
                            response: snapshot,
                        },
                    );
                }
                ResponsesOutput::Json(Box::new(obj))
            }
        };

        Ok(ChatResult {
            data,
            account_id,
            prompt_tokens,
        })
    }

    /// GET /v1/responses/{id}
    ///
    /// 返回创建时保存的完整 Response 对象快照；未命中（不存在 / 已被容量淘汰 /
    /// 超过 TTL）返回 `None`，由 HTTP 层映射为 404。
    ///
    /// 注意：快照保存在**进程内**（有界 + TTL），重启即失效 —— 与
    /// `previous_response_id` 的存储语义一致。
    #[must_use]
    pub fn get_response(&self, id: &str) -> Option<serde_json::Value> {
        self.store.get_response(id)
    }

    /// 按请求参数构造响应上下文（同时用于流式与非流式）
    fn build_ctx(req: &ResponsesRequest) -> ResponseCtx {
        let tools = req
            .tools
            .as_deref()
            .map(|tools| tools.iter().map(echo_tool).collect())
            .unwrap_or_default();

        ResponseCtx {
            id: response::next_response_id(),
            model: req.model.clone(),
            instructions: request::instructions_text(req.instructions.as_ref()),
            max_output_tokens: req.max_output_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            tools,
            tool_choice: req
                .tool_choice
                .clone()
                .unwrap_or_else(|| serde_json::Value::String("auto".to_string())),
            parallel_tool_calls: req.parallel_tool_calls.unwrap_or(true),
            reasoning_effort: req.reasoning.as_ref().and_then(|r| r.effort.clone()),
            reasoning_summary: req.reasoning.as_ref().and_then(|r| r.summary.clone()),
            store: req.store.unwrap_or(true),
            previous_response_id: req.previous_response_id.clone(),
            metadata: req
                .metadata
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
            user: req.user.clone(),
        }
    }
}

/// 回显工具定义（统一为 Responses 的扁平结构）
fn echo_tool(t: &types::ResponsesTool) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("type".to_string(), serde_json::Value::String(t.ty.clone()));

    let name = t
        .name
        .clone()
        .or_else(|| t.function.as_ref().map(|f| f.name.clone()));
    if let Some(name) = name {
        obj.insert("name".to_string(), serde_json::Value::String(name));
    }
    let description = t
        .description
        .clone()
        .or_else(|| t.function.as_ref().and_then(|f| f.description.clone()));
    if let Some(d) = description {
        obj.insert("description".to_string(), serde_json::Value::String(d));
    }
    let parameters = t
        .parameters
        .clone()
        .or_else(|| t.function.as_ref().map(|f| f.parameters.clone()));
    if let Some(p) = parameters {
        obj.insert("parameters".to_string(), p);
    }
    let strict = t
        .strict
        .or_else(|| t.function.as_ref().and_then(|f| f.strict));
    if let Some(s) = strict {
        obj.insert("strict".to_string(), serde_json::Value::Bool(s));
    }
    serde_json::Value::Object(obj)
}

/// 提取本轮用户输入文本（用于 `previous_response_id` 重建上下文）
fn extract_input_text(req: &ResponsesRequest) -> Option<String> {
    use types::{Input, InputContent, InputContentPart, InputItem};

    match req.input.as_ref()? {
        Input::Text(t) => Some(t.clone()),
        Input::Items(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let InputItem::Message { role, content } = item
                    && role == "user"
                {
                    match content {
                        InputContent::Text(t) => parts.push(t.clone()),
                        InputContent::Parts(blocks) => {
                            for block in blocks {
                                if let InputContentPart::Text(t) = block {
                                    parts.push(t.clone());
                                }
                            }
                        }
                    }
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ResponsesRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn extract_text_from_string_input() {
        let req = parse(r#"{"model":"m","input":"hello"}"#);
        assert_eq!(extract_input_text(&req).as_deref(), Some("hello"));
    }

    #[test]
    fn extract_text_ignores_assistant_and_tool_items() {
        let req = parse(
            r#"{"model":"m","input":[
                {"role":"assistant","content":"old"},
                {"role":"user","content":"new"},
                {"type":"function_call","call_id":"c","name":"f","arguments":"{}"}
            ]}"#,
        );
        assert_eq!(extract_input_text(&req).as_deref(), Some("new"));
    }

    #[test]
    fn extract_text_joins_user_text_parts() {
        let req = parse(
            r#"{"model":"m","input":[{"role":"user","content":[
                {"type":"input_text","text":"a"},
                {"type":"input_image","image_url":"http://x/y.png"}
            ]}]}"#,
        );
        assert_eq!(extract_input_text(&req).as_deref(), Some("a"));
    }

    #[test]
    fn tool_echo_flattens_nested_form() {
        let req = parse(
            r#"{"model":"m","tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"},"strict":true}}]}"#,
        );
        let echo = echo_tool(&req.tools.unwrap()[0]);
        assert_eq!(echo["type"], "function");
        assert_eq!(echo["name"], "f");
        assert_eq!(echo["strict"], true);
        assert!(echo.get("function").is_none());
    }

    #[test]
    fn store_defaults_to_enabled() {
        // Responses API 语义：store 默认 true
        let req = parse(r#"{"model":"m","input":"hi"}"#);
        assert!(req.store.is_none());
    }
}

//! OpenAI Responses API 协议类型定义
//!
//! 对齐 `POST /v1/responses`（OpenAI Responses API，2025 版）。
//! 参考实现来源：openai-openapi 的 `CreateResponse` / `InputItem` /
//! `OutputItem` / `ResponseStreamEvent` schema，以及 openai-python
//! `openai/lib/streaming/responses/`。
//!
//! 设计原则与 `openai_adapter/types.rs` 一致：接口层面全对齐，
//! 无法实现的字段解析后忽略（providers-only 字段不消费）。

use serde::{Deserialize, Serialize};

// ============================================================================
// 请求类型
// ============================================================================

/// POST /v1/responses 请求体
#[derive(Debug, Deserialize, Default)]
pub struct ResponsesRequest {
    pub model: String,

    /// 输入：纯文本，或输入项数组
    #[serde(default)]
    pub input: Option<Input>,

    /// 系统（developer）指令
    #[serde(default)]
    pub instructions: Option<Instructions>,

    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<ReasoningParam>,
    #[serde(default)]
    pub text: Option<TextParam>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub stream_options: Option<ResponsesStreamOptions>,

    // ── 以下字段解析但不消费（保留字段以对齐协议 & 供后续扩展）──
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    #[serde(default)]
    pub prompt: Option<serde_json::Value>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub prompt_cache_retention: Option<String>,
    #[serde(default)]
    pub safety_identifier: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub top_logprobs: Option<u8>,
    #[serde(default)]
    pub truncation: Option<String>,

    // 兜底：未知字段直接忽略
    #[serde(flatten)]
    pub _extra: serde_json::Value,
}

/// `instructions` 支持纯字符串；数组形式按文本项拼接
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Instructions {
    Text(String),
    Items(Vec<serde_json::Value>),
}

impl Instructions {
    /// 展平为纯文本，多个文本块以换行连接
    #[must_use]
    pub fn flatten(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Items(items) => items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                        .or_else(|| {
                            item.get("content").and_then(|c| {
                                c.as_str().map(str::to_string).or_else(|| {
                                    c.as_array().map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    })
                                })
                            })
                        })
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// `input` 参数：文本 或 输入项数组
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<InputItem>),
}

/// 输入项
///
/// 自定义反序列化：`type` 在 `EasyInputMessage` 上是可选的，
/// 而 `function_call` / `function_call_output` 等以 `type` 判别，
/// 因此直接读取原始 JSON 后再分类，避免 untagged 枚举的歧义匹配。
#[derive(Debug, Clone)]
pub enum InputItem {
    /// role + content 的消息项（user / assistant / system / developer）
    Message { role: String, content: InputContent },
    /// 上一轮模型产生的函数调用
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// 函数调用结果（客户端回填）
    FunctionCallOutput { call_id: String, output: String },
    /// 引用此前响应中的某个 output item
    ItemReference { id: String },
    /// 其余类型（reasoning / item_reference / 服务器工具调用等）暂不消费
    Other,
}

/// 消息内容：文本 或 内容块数组
#[derive(Debug, Clone)]
pub enum InputContent {
    Text(String),
    Parts(Vec<InputContentPart>),
}

/// 输入内容块（已归一化为内部需要的三种形态）
#[derive(Debug, Clone)]
pub enum InputContentPart {
    Text(String),
    ImageUrl {
        url: String,
        detail: Option<String>,
    },
    File {
        file_data: Option<String>,
        file_id: Option<String>,
        filename: Option<String>,
    },
}

impl<'de> Deserialize<'de> for InputItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let Some(obj) = value.as_object() else {
            return Ok(Self::Other);
        };

        let ty = obj.get("type").and_then(|v| v.as_str());

        // role 存在即视为消息项（type 字段在该形态下是可选的）
        if obj.contains_key("role") || ty == Some("message") {
            let role = obj
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            let content = obj
                .get("content")
                .map_or(InputContent::Text(String::new()), parse_input_content);
            return Ok(Self::Message { role, content });
        }

        match ty {
            Some("function_call") => Ok(Self::FunctionCall {
                call_id: string_field(obj, "call_id"),
                name: string_field(obj, "name"),
                arguments: match obj.get("arguments") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                },
            }),
            Some("function_call_output") => Ok(Self::FunctionCallOutput {
                call_id: string_field(obj, "call_id"),
                output: match obj.get("output") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                },
            }),
            Some("item_reference") => Ok(Self::ItemReference {
                id: string_field(obj, "id"),
            }),
            _ => Ok(Self::Other),
        }
    }
}

fn string_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    obj.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn parse_input_content(value: &serde_json::Value) -> InputContent {
    match value {
        serde_json::Value::String(s) => InputContent::Text(s.clone()),
        serde_json::Value::Array(items) => InputContent::Parts(
            items
                .iter()
                .filter_map(|item| {
                    let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                    match ty {
                        "input_text" | "output_text" | "text" => Some(InputContentPart::Text(
                            item.get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        )),
                        "input_image" | "image_url" => item
                            .get("image_url")
                            .and_then(|v| {
                                v.as_str().map(str::to_string).or_else(|| {
                                    v.get("url").and_then(|u| u.as_str()).map(str::to_string)
                                })
                            })
                            .or_else(|| {
                                item.get("image_url")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string)
                            })
                            .map(|url| InputContentPart::ImageUrl {
                                url,
                                detail: item
                                    .get("detail")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string),
                            }),
                        "input_file" => Some(InputContentPart::File {
                            file_data: item
                                .get("file_data")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            file_id: item
                                .get("file_id")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            filename: item
                                .get("filename")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                        }),
                        _ => None,
                    }
                })
                .collect(),
        ),
        _ => InputContent::Text(String::new()),
    }
}

/// 工具定义
///
/// Responses API 使用扁平结构（`{type, name, description, parameters}`），
/// 但部分网关/客户端仍发 Chat Completions 的嵌套结构，
/// 这里两种都接受（见 `normalize_tool`）。
#[derive(Debug, Deserialize, Clone)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    #[serde(default)]
    pub strict: Option<bool>,
    /// 兼容 Chat Completions 的嵌套形态
    #[serde(default)]
    pub function: Option<NestedFunction>,
    /// 兼容 custom tool 形态
    #[serde(default)]
    pub format: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NestedFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub strict: Option<bool>,
}

/// `reasoning` 参数
#[derive(Debug, Deserialize, Clone)]
pub struct ReasoningParam {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    /// 返回字段（请求侧无意义）
    #[serde(default)]
    pub mode: Option<String>,
}

/// `text` 参数
#[derive(Debug, Deserialize, Clone, Default)]
pub struct TextParam {
    #[serde(default)]
    pub format: Option<TextFormat>,
    #[serde(default)]
    pub verbosity: Option<String>,
}

/// `text.format`
#[derive(Debug, Deserialize, Clone)]
pub struct TextFormat {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    #[serde(default)]
    pub strict: Option<bool>,
}

/// `stream_options`
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ResponsesStreamOptions {
    #[serde(default)]
    pub include_obfuscation: Option<bool>,
}

// ============================================================================
// 响应类型
// ============================================================================

/// Response 对象（`object: "response"`）
#[derive(Debug, Serialize, Clone)]
pub struct ResponseObject {
    pub id: String,
    pub object: &'static str,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    pub status: &'static str,
    pub error: Option<ResponseError>,
    pub incomplete_details: Option<IncompleteDetails>,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub model: String,
    pub output: Vec<serde_json::Value>,
    /// SDK 便利字段：聚合所有 `output_text` 块
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_text: Option<String>,
    pub parallel_tool_calls: bool,
    pub previous_response_id: Option<String>,
    pub reasoning: ReasoningSummary,
    pub store: bool,
    pub temperature: Option<f32>,
    pub text: TextSummary,
    pub tool_choice: serde_json::Value,
    pub tools: Vec<serde_json::Value>,
    pub top_p: Option<f32>,
    pub truncation: String,
    pub usage: Option<ResponseUsage>,
    pub user: Option<String>,
    pub metadata: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// 响应内嵌错误对象
#[derive(Debug, Serialize, Clone)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

/// 未完成原因
#[derive(Debug, Serialize, Clone)]
pub struct IncompleteDetails {
    pub reason: &'static str,
}

/// 响应中的 `reasoning` 回显
#[derive(Debug, Serialize, Clone)]
pub struct ReasoningSummary {
    pub effort: Option<String>,
    pub summary: Option<String>,
}

/// 响应中的 `text` 回显
#[derive(Debug, Serialize, Clone)]
pub struct TextSummary {
    pub format: serde_json::Value,
}

/// usage（注意：与 Chat Completions 的字段名不同）
#[derive(Debug, Serialize, Clone, Default)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub input_tokens_details: InputTokensDetails,
    pub output_tokens: u32,
    pub output_tokens_details: OutputTokensDetails,
    pub total_tokens: u32,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct InputTokensDetails {
    pub cached_tokens: u32,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u32,
}

// ============================================================================
// 流式事件载荷
// ============================================================================

/// 流式事件：`(event_name, data_json)`
///
/// 由于事件种类多、字段差异大，直接以 `serde_json::Value` 承载 data，
/// 避免为每个事件定义独立结构体（17+ 个）。
pub type SseEvent = (&'static str, serde_json::Value);

/// 序列化为 `event: <name>\ndata: <json>\n\n`
#[must_use]
/// 序列化一个 Responses SSE 事件：`event: <name>\ndata: <json>\n\n`
///
/// 直接写入同一个缓冲区：省掉「JSON 字符串 + format! 再拼一份」的双重分配。
pub fn sse_bytes(event: &SseEvent) -> bytes::Bytes {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"event: ");
    buf.extend_from_slice(event.0.as_bytes());
    buf.extend_from_slice(b"\ndata: ");
    if serde_json::to_writer(&mut buf, &event.1).is_err() {
        buf.extend_from_slice(b"{}");
    }
    buf.extend_from_slice(b"\n\n");
    bytes::Bytes::from(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_string_form() {
        let req: ResponsesRequest =
            serde_json::from_str(r#"{"model":"deepseek-default","input":"hello"}"#).unwrap();
        match req.input {
            Some(Input::Text(t)) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn input_message_items_without_type_field() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","input":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"output_text","text":"yo"}]}]}"#,
        )
        .unwrap();
        let Some(Input::Items(items)) = req.input else {
            panic!("expected items");
        };
        assert_eq!(items.len(), 2);
        match &items[0] {
            InputItem::Message { role, content } => {
                assert_eq!(role, "user");
                match content {
                    InputContent::Text(t) => assert_eq!(t, "hi"),
                    other @ InputContent::Parts(_) => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected message, got {other:?}"),
        }
        match &items[1] {
            InputItem::Message { role, content } => {
                assert_eq!(role, "assistant");
                match content {
                    InputContent::Parts(parts) => assert_eq!(parts.len(), 1),
                    other @ InputContent::Text(_) => panic!("expected parts, got {other:?}"),
                }
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn input_function_call_roundtrip() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","input":[
                {"type":"function_call","call_id":"call_1","name":"f","arguments":"{\"a\":1}"},
                {"type":"function_call_output","call_id":"call_1","output":"42"}
            ]}"#,
        )
        .unwrap();
        let Some(Input::Items(items)) = req.input else {
            panic!("expected items");
        };
        match &items[0] {
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "f");
                assert_eq!(arguments, r#"{"a":1}"#);
            }
            other => panic!("expected function_call, got {other:?}"),
        }
        match &items[1] {
            InputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(output, "42");
            }
            other => panic!("expected function_call_output, got {other:?}"),
        }
    }

    #[test]
    fn instructions_array_flattened() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","instructions":[{"type":"input_text","text":"a"},{"type":"input_text","text":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(req.instructions.unwrap().flatten(), "a\nb");
    }

    #[test]
    fn tool_flat_and_nested_forms() {
        let req: ResponsesRequest = serde_json::from_str(
            r#"{"model":"m","tools":[
                {"type":"function","name":"flat","parameters":{"type":"object"}},
                {"type":"function","function":{"name":"nested","parameters":{"type":"object"}}},
                {"type":"web_search_preview"}
            ]}"#,
        )
        .unwrap();
        let tools = req.tools.unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name.as_deref(), Some("flat"));
        assert_eq!(tools[1].function.as_ref().unwrap().name, "nested");
        assert_eq!(tools[2].ty, "web_search_preview");
    }

    #[test]
    fn empty_request_uses_defaults() {
        let req: ResponsesRequest = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert!(req.input.is_none());
        assert!(!req.stream);
    }
}

//! Anthropic Messages API 请求类型定义
//!
//! 对齐 Anthropic Messages API 协议，保留全部兼容字段。
//! 未消费的字段通过 `pub` 字段避免编译器 warning，与 openai_adapter/types.rs 对称。

use bytes::Bytes;
use log::trace;
use serde::Deserialize;
use serde::Serialize;

// ============================================================================
// 顶层请求
// ============================================================================

/// POST /v1/messages 请求体
#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<MessageParam>,
    pub max_tokens: u32,

    #[serde(default)]
    pub system: Option<SystemContent>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub tools: Option<Vec<ToolUnion>>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
    #[serde(default)]
    pub output_config: Option<OutputConfig>,
    /// 智能搜索选项（Anthropic 协议扩展字段，映射为 OpenAI web_search_options）
    #[serde(default)]
    pub web_search_options: Option<serde_json::Value>,

    // 兼容字段：解析但不消费
    #[serde(default)]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub inference_geo: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,

    // 兜底
    #[serde(flatten)]
    pub _extra: serde_json::Value,
}

// ============================================================================
// 消息
// ============================================================================

/// 消息参数
#[derive(Debug, Deserialize, Clone)]
pub struct MessageParam {
    pub role: String,
    pub content: MessageContent,
}

/// 消息内容：纯文本或内容块数组
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// 内容块
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    Document {
        source: ImageSource,
        #[serde(default)]
        title: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<ToolResultContent>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    // 其他类型（search_result / server_tool_use 等）直接忽略
    #[serde(other)]
    Other,
}

/// 图片源
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ImageSource {
    #[serde(rename = "base64")]
    Base64 { data: String, media_type: String },
    #[serde(rename = "url")]
    Url { url: String },
}

/// tool_result 内容：字符串或块数组
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// system 参数：字符串或文本块数组
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<SystemTextBlock>),
}

/// system 文本块（仅提取 text，忽略 cache_control / citations）
#[derive(Debug, Deserialize, Clone)]
pub struct SystemTextBlock {
    pub text: String,
    #[serde(rename = "type")]
    pub ty: String,
}

// ============================================================================
// 工具
// ============================================================================

/// 工具联合类型
#[derive(Debug, Clone)]
pub enum ToolUnion {
    Custom {
        name: String,
        description: Option<String>,
        input_schema: serde_json::Value,
        strict: Option<bool>,
    },
    // 服务器工具（bash / code_execution / web_search 等）忽略
    Other,
}

impl<'de> serde::Deserialize<'de> for ToolUnion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("tool must be an object"))?;

        match obj.get("type").and_then(|v| v.as_str()) {
            Some("custom") | None => {
                let name = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| serde::de::Error::missing_field("name"))?;
                let description = obj
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let input_schema = obj.get("input_schema").cloned().unwrap_or_default();
                let strict = obj.get("strict").and_then(|v| v.as_bool());
                Ok(ToolUnion::Custom {
                    name,
                    description,
                    input_schema,
                    strict,
                })
            }
            Some(_) => Ok(ToolUnion::Other),
        }
    }
}

/// tool_choice 参数
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ToolChoice {
    #[serde(rename = "auto")]
    Auto {
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    #[serde(rename = "any")]
    Any {
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    #[serde(rename = "tool")]
    Tool {
        name: String,
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    #[serde(rename = "none")]
    None,
}

// ============================================================================
// 思考 / 输出控制 / 元数据
// ============================================================================

/// thinking 配置
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ThinkingConfig {
    #[serde(rename = "enabled")]
    Enabled {
        budget_tokens: u32,
        #[serde(default)]
        display: Option<String>,
    },
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(default)]
        display: Option<String>,
    },
}

/// 请求元数据
#[derive(Debug, Deserialize, Clone)]
pub struct Metadata {
    #[serde(default)]
    pub user_id: Option<String>,
}

/// 输出配置
#[derive(Debug, Deserialize, Clone)]
pub struct OutputConfig {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub format: Option<JsonOutputFormat>,
}

/// JSON 输出格式
#[derive(Debug, Deserialize, Clone)]
pub struct JsonOutputFormat {
    pub schema: serde_json::Value,
    #[serde(rename = "type")]
    pub ty: String,
}

/// cache_control（兼容解析）
#[derive(Debug, Deserialize, Clone)]
pub struct CacheControlEphemeral {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub ttl: Option<String>,
}

// ============================================================================
// 响应类型
// ============================================================================

/// Anthropic 非流式消息响应（流式的 message_start 也复用此结构）
#[derive(Debug, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub role: &'static str,
    pub model: String,
    pub content: Vec<ResponseContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

/// Content block 变体（响应侧：只包含模型能输出的类型）
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ResponseContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Token 用量
#[derive(Debug, Serialize, Clone)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ============================================================================
// 流式 chunk（对应 OpenAI 的 ChatCompletionsResponseChunk）
// ============================================================================

/// Content block delta 变体
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

/// Anthropic 流式响应 chunk（对标 ChatCompletionsResponseChunk）
pub enum MessagesResponseChunk {
    MessageStart {
        message: MessagesResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: ResponseContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentBlockDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    /// 心跳事件：Anthropic 协议规定的 `ping`，客户端应忽略其内容
    Ping,
    MessageDelta {
        stop_reason: Option<String>,
        stop_sequence: Option<String>,
        output_tokens: Option<u32>,
    },
    MessageStop,
}

impl MessagesResponseChunk {
    /// Extract output_tokens from MessageDelta events
    #[must_use]
    pub const fn output_tokens(&self) -> Option<u32> {
        match self {
            Self::MessageDelta { output_tokens, .. } => *output_tokens,
            _ => None,
        }
    }

    #[must_use]
    pub const fn event_name(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::Ping => "ping",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
        }
    }

    /// 序列化为 Anthropic SSE 事件格式：event: xxx\ndata: {json}\n\n
    pub fn to_sse_bytes(&self) -> Result<Bytes, serde_json::Error> {
        let json = match self {
            Self::MessageStart { message } => serde_json::to_string(&serde_json::json!({
                "type": "message_start",
                "message": message,
            }))?,
            Self::ContentBlockStart {
                index,
                content_block,
            } => serde_json::to_string(&serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block,
            }))?,
            Self::ContentBlockDelta { index, delta } => {
                serde_json::to_string(&serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": delta,
                }))?
            }
            Self::ContentBlockStop { index } => serde_json::to_string(&serde_json::json!({
                "type": "content_block_stop",
                "index": index,
            }))?,
            Self::Ping => serde_json::to_string(&serde_json::json!({
                "type": "ping",
            }))?,
            Self::MessageDelta {
                stop_reason,
                stop_sequence,
                output_tokens,
            } => {
                let mut obj = serde_json::json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": stop_reason,
                        "stop_sequence": stop_sequence,
                    },
                });
                if let Some(tokens) = output_tokens {
                    obj["usage"] = serde_json::json!({"output_tokens": tokens});
                }
                serde_json::to_string(&obj)?
            }
            Self::MessageStop => serde_json::to_string(&serde_json::json!({
                "type": "message_stop",
            }))?,
        };
        let sse = format!("event: {}\ndata: {}\n\n", self.event_name(), json);
        trace!(target: "anthropic_compat::response::stream", ">>> {}", sse.trim());
        Ok(Bytes::from(sse))
    }
}

//! Anthropic 协议兼容层 —— 基于 openai_adapter 提供 Anthropic API 兼容接口
//!
//! 本模块不直接访问 ds_core，所有数据通过 openai_adapter 获取并做格式映射。
//! 请求流向：Anthropic JSON → ChatCompletionsRequest → openai_adapter → 响应映射回 Anthropic 格式。

mod models;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod types;

pub use types::{MessagesRequest, MessagesResponse, MessagesResponseChunk};

/// Anthropic 流式响应类型（结构体流）
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<MessagesResponseChunk, AnthropicCompatError>> + Send>>;

/// Anthropic 流式响应类型（SSE 字节流）
pub type StreamResponse = Pin<Box<dyn Stream<Item = Result<Bytes, AnthropicCompatError>> + Send>>;

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use log::debug;

use crate::openai_adapter::{ChatOutput, ChatResult, OpenAIAdapter, OpenAIAdapterError};

/// Anthropic 统一输出（对标 openai_adapter 的 ChatOutput）
pub enum AnthropicOutput {
    Stream(ChunkStream),
    Json(MessagesResponse),
}

/// Anthropic 兼容层
pub struct AnthropicCompat {
    openai_adapter: Arc<OpenAIAdapter>,
}

impl AnthropicCompat {
    /// 创建兼容层实例
    pub const fn new(openai_adapter: Arc<OpenAIAdapter>) -> Self {
        Self { openai_adapter }
    }

    /// POST /v1/messages（统一入口）
    ///
    /// 将 Anthropic 请求映射为 ChatCompletionsRequest，委托给 openai_adapter，
    /// 返回时再按 OpenAI 的 stream 分流结果映射回 Anthropic 格式。
    pub async fn messages(
        &self,
        req: MessagesRequest,
        request_id: &str,
    ) -> Result<ChatResult<AnthropicOutput>, AnthropicCompatError> {
        debug!(target: "anthropic_compat", "received messages request");
        let chat_req = request::into_chat_completions(req);
        let result = self
            .openai_adapter
            .chat_completions(chat_req, request_id)
            .await?;
        let data = match result.data {
            ChatOutput::Stream(stream) => {
                AnthropicOutput::Stream(response::from_chat_completion_stream(stream))
            }
            ChatOutput::Json(json) => {
                let msg = response::from_chat_completions(&json);
                AnthropicOutput::Json(msg)
            }
        };
        Ok(ChatResult {
            data,
            account_id: result.account_id,
            prompt_tokens: result.prompt_tokens,
        })
    }

    /// GET /v1/models
    ///
    /// 返回 Anthropic 格式的模型列表。
    pub async fn list_models(&self) -> models::AnthropicModelList {
        debug!(target: "anthropic_compat", "received model list request");
        models::list(&self.openai_adapter.list_models().await)
    }

    /// GET /v1/models/{model_id}
    ///
    /// 返回指定模型的 Anthropic 格式详情。
    pub async fn get_model(&self, model_id: &str) -> Option<models::AnthropicModel> {
        debug!(target: "anthropic_compat", "lookup model: {model_id}");
        models::get(&self.openai_adapter.list_models().await, model_id)
    }
}

/// Anthropic 兼容层错误类型
#[derive(Debug, thiserror::Error)]
pub enum AnthropicCompatError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("service overloaded")]
    Overloaded,
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<OpenAIAdapterError> for AnthropicCompatError {
    fn from(e: OpenAIAdapterError) -> Self {
        match e {
            OpenAIAdapterError::BadRequest(msg) => Self::BadRequest(msg),
            OpenAIAdapterError::Overloaded => Self::Overloaded,
            OpenAIAdapterError::ProviderError(msg)
            | OpenAIAdapterError::Internal(msg)
            | OpenAIAdapterError::ToolCallRepairNeeded(msg) => Self::Internal(msg),
        }
    }
}

impl AnthropicCompatError {
    /// 返回对应 HTTP 状态码
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Overloaded => 429,
            Self::Internal(_) => 500,
        }
    }
}

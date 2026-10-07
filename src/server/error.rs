//! HTTP 错误响应格式 —— 支持 OpenAI 与 Anthropic 兼容错误 JSON
//!
//! 将适配器错误映射为标准错误响应格式。

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::fmt;

use crate::anthropic_compat::AnthropicCompatError;
use crate::openai_adapter::OpenAIAdapterError;

/// OpenAI 兼容错误响应体
#[derive(Debug, Serialize)]
pub struct OpenAIErrorBody {
    error: OpenAIErrorDetail,
}

/// OpenAI 错误明细：`Error` schema 要求 `type`/`message`/`param`/`code` 四个字段齐备
#[derive(Debug, Serialize)]
struct OpenAIErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    param: Option<String>,
    code: &'static str,
}

/// Anthropic 兼容错误响应体
///
/// 规范形态是 `{"type":"error","error":{"type":"<kind>","message":"..."}}`，
/// 而不是把 error kind 放在顶层。
#[derive(Debug, Serialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    outer_type: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(Debug, Serialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
}

/// 服务器层错误类型
#[derive(Debug)]
pub enum ServerError {
    /// OpenAI 适配器错误
    Adapter(OpenAIAdapterError),
    /// Anthropic 兼容层错误
    Anthropic(AnthropicCompatError),
    /// 未授权（无效 API token）
    Unauthorized,
    /// 资源不存在
    NotFound(String),
    /// 幂等键被复用在不同请求体上（400）
    IdempotencyConflict,
    /// 同幂等键的请求正在执行（409）
    IdempotencyInProgress,
    /// 同幂等键的历史响应不可回放（超限 / 上次中断，409）
    IdempotencyUnreplayable,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(e) => write!(f, "{e}"),
            Self::Anthropic(e) => write!(f, "{e}"),
            Self::Unauthorized => write!(f, "invalid api token"),
            Self::NotFound(id) => write!(f, "模型 '{id}' 不存在"),
            Self::IdempotencyConflict => write!(
                f,
                "Idempotency-Key 已用于不同的请求体；如需复用请使用新的 key"
            ),
            Self::IdempotencyInProgress => write!(
                f,
                "相同 Idempotency-Key 的请求正在处理中，请稍后重试或改用新的 key"
            ),
            Self::IdempotencyUnreplayable => write!(
                f,
                "相同 Idempotency-Key 的历史响应不可回放（响应体超限或上次未完整结束）"
            ),
        }
    }
}

impl From<OpenAIAdapterError> for ServerError {
    fn from(e: OpenAIAdapterError) -> Self {
        Self::Adapter(e)
    }
}

impl From<AnthropicCompatError> for ServerError {
    fn from(e: AnthropicCompatError) -> Self {
        Self::Anthropic(e)
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match &self {
            Self::Anthropic(e) => anthropic_error_response(e),
            // `/anthropic/*` 上的幂等错误必须用 Anthropic 信封，否则 SDK 无法归类
            Self::IdempotencyConflict
            | Self::IdempotencyInProgress
            | Self::IdempotencyUnreplayable => anthropic_idempotency_response(&self),
            _ => openai_error_response(&self),
        }
    }
}

/// Anthropic 形态的幂等错误（400 / 409）
fn anthropic_idempotency_response(err: &ServerError) -> Response {
    let status = if matches!(err, ServerError::IdempotencyConflict) {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::CONFLICT
    };
    let body = AnthropicErrorBody {
        outer_type: "error",
        error: AnthropicErrorDetail {
            error_type: "invalid_request_error",
            message: err.to_string(),
        },
    };
    log::debug!(
        target: "http::response",
        "{} Anthropic idempotency error: {}", status, body.error.message
    );
    (status, Json(body)).into_response()
}

fn openai_error_response(err: &ServerError) -> Response {
    let (status, error_type, code) = match err {
        ServerError::Adapter(e) => {
            let status =
                StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let (error_type, code) = match e {
                OpenAIAdapterError::BadRequest(_) => ("invalid_request_error", "bad_request"),
                OpenAIAdapterError::Overloaded => ("server_error", "overloaded"),
                OpenAIAdapterError::ProviderError(_) => ("server_error", "provider_error"),
                OpenAIAdapterError::Internal(_) | OpenAIAdapterError::ToolCallRepairNeeded(_) => {
                    ("server_error", "internal_error")
                }
            };
            (status, error_type, code)
        }
        // 对齐 OpenAI 官方 401 响应：`invalid_request_error` + `invalid_api_key`
        ServerError::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "invalid_request_error",
            "invalid_api_key",
        ),
        ServerError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
        ),
        // 幂等键冲突属于客户端用法错误：code 对齐 Stripe / OpenAI 的 `idempotency_error`
        ServerError::IdempotencyConflict => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "idempotency_error",
        ),
        ServerError::IdempotencyInProgress | ServerError::IdempotencyUnreplayable => (
            StatusCode::CONFLICT,
            "invalid_request_error",
            "idempotency_error",
        ),
        // Anthropic 错误不会走到这里
        ServerError::Anthropic(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "internal_error",
        ),
    };

    let body = OpenAIErrorBody {
        error: OpenAIErrorDetail {
            message: err.to_string(),
            error_type,
            param: None,
            code,
        },
    };

    log::debug!(target: "http::response", "{} error: {}", status, body.error.message);

    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
    }
    resp
}

fn anthropic_error_response(err: &AnthropicCompatError) -> Response {
    let status =
        StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let error_type = match err {
        AnthropicCompatError::BadRequest(_) => "invalid_request_error",
        AnthropicCompatError::Overloaded => "overloaded_error",
        AnthropicCompatError::Internal(_) => "api_error",
    };

    let body = AnthropicErrorBody {
        outer_type: "error",
        error: AnthropicErrorDetail {
            error_type,
            message: err.to_string(),
        },
    };

    log::debug!(
        target: "http::response",
        "{} Anthropic error: {}", status, body.error.message
    );

    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
    }
    resp
}

/// Anthropic 形态的鉴权/未找到错误（供 `/anthropic/*` 路由使用）
///
/// 中间件无法访问 handler 的 `ServerError`，因此这里提供独立构造函数，
/// 保证 Anthropic 客户端收到规范的错误信封而不是 OpenAI 形态。
#[must_use]
pub fn anthropic_auth_error() -> Response {
    let body = AnthropicErrorBody {
        outer_type: "error",
        error: AnthropicErrorDetail {
            error_type: "authentication_error",
            message: "invalid api token".to_string(),
        },
    };
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

/// Anthropic 形态的 404
#[must_use]
pub fn anthropic_not_found_error(message: &str) -> Response {
    let body = AnthropicErrorBody {
        outer_type: "error",
        error: AnthropicErrorDetail {
            error_type: "not_found_error",
            message: message.to_string(),
        },
    };
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

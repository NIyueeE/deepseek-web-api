//! HTTP 路由处理器 —— 薄路由层，委托给 OpenAIAdapter / AnthropicCompat
//!
//! 所有业务逻辑在 adapter 中，handler 只做参数提取和响应格式化。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    body::Body,
    extract::{FromRequestParts, Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::anthropic_compat::{
    AnthropicCompat, AnthropicCompatError, AnthropicOutput, MessagesRequest,
};
use crate::config::Config;
use crate::openai_adapter::{
    ChatCompletionsRequest, ChatOutput, OpenAIAdapter, OpenAIAdapterError,
};
use crate::responses_adapter::{ResponsesAdapter, ResponsesOutput, ResponsesRequest};

use super::auth::LoginLimiter;
use super::error::ServerError;
use super::idempotency::{
    self, Begin, IdempotencyGuard, IdempotencyStore, RecordedResponse, RecordingStream,
};
use super::mask_prefix;
use super::stats::Stats;
use super::store::StoreManager;
use super::stream::SseBody;

/// Extract the API key from request extensions (injected by api_key_middleware)
pub(crate) struct ApiKey(pub(crate) Option<String>);

impl<S> FromRequestParts<S> for ApiKey
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let key = parts
            .extensions
            .get::<super::ApiKeyExt>()
            .map(|e| e.0.clone());
        Ok(ApiKey(key))
    }
}

/// Guard that records token usage to Stats on Drop
struct TokenGuard {
    stats: Arc<Stats>,
    prompt_tokens: u64,
    completion_tokens: Arc<std::sync::atomic::AtomicU64>,
    model: String,
    api_key: Option<String>,
    request_id: String,
    latency_ms: u64,
    success: bool,
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        let ct = self
            .completion_tokens
            .load(std::sync::atomic::Ordering::Relaxed);
        self.stats.record_tokens_for_model_and_key(
            &self.model,
            self.api_key.as_deref(),
            self.prompt_tokens,
            ct,
        );
        // Append request log asynchronously
        let stats = self.stats.clone();
        let log = super::stats::RequestLog {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            request_id: self.request_id.clone(),
            model: self.model.clone(),
            api_key: self
                .api_key
                .as_deref()
                .map(|k| mask_prefix(k, 8))
                .unwrap_or_default(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: ct,
            latency_ms: self.latency_ms,
            success: self.success,
        };
        tokio::spawn(async move {
            stats.append_log(log);
        });
    }
}

pin_project! {
    /// Stream wrapper that holds a TokenGuard; guard fires on Drop (stream end)
    struct TokenGuardStream<S> {
        #[pin]
        inner: S,
        _guard: TokenGuard,
    }
}

impl<S, E> Stream for TokenGuardStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_request_id() -> String {
    format!("req-{:x}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

const X_DS_ACCOUNT: &str = "x-ds-account";

/// Anthropic 规范建议每个响应携带本次请求 ID
const ANTHROPIC_REQUEST_ID: &str = "request-id";

/// 脱敏账号 ID：邮箱/手机号只保留前 3 字符 + ***
fn mask_account_id(id: &str) -> String {
    mask_prefix(id, 3)
}

/// 应用状态
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) adapter: Arc<OpenAIAdapter>,
    pub(crate) anthropic_compat: Arc<AnthropicCompat>,
    pub(crate) responses_adapter: Arc<ResponsesAdapter>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) config: Arc<tokio::sync::RwLock<Config>>,
    pub(crate) store: Arc<StoreManager>,
    pub(crate) login_limiter: Arc<LoginLimiter>,
    pub(crate) config_path: PathBuf,
    /// `Idempotency-Key` 缓存（进程内，有界 + TTL）
    pub(crate) idempotency: Arc<IdempotencyStore>,
}
struct RequestRecord<'a> {
    request_id: &'a str,
    model: &'a str,
    api_key: &'a Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    latency_ms: u64,
    success: bool,
}

/// Record a completed request — logs tokens and appends RequestLog via Stats
impl AppState {
    fn record_request(&self, rec: &RequestRecord<'_>) {
        self.stats.record_tokens_for_model_and_key(
            rec.model,
            rec.api_key.as_deref(),
            rec.prompt_tokens,
            rec.completion_tokens,
        );
        let api_key_masked = rec
            .api_key
            .as_deref()
            .map(|k| mask_prefix(k, 8))
            .unwrap_or_default();
        let log = super::stats::RequestLog {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            request_id: rec.request_id.to_string(),
            model: rec.model.to_string(),
            api_key: api_key_masked,
            prompt_tokens: rec.prompt_tokens,
            completion_tokens: rec.completion_tokens,
            latency_ms: rec.latency_ms,
            success: rec.success,
        };
        let stats = self.stats.clone();
        tokio::spawn(async move {
            stats.append_log(log);
        });
    }
}

// ── 幂等性（Idempotency-Key）───────────────────────────────────────────

/// 客户端幂等键请求头（Stripe / OpenAI 约定）
const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
/// 回放响应标记：客户端可据此判断「这次没有真的打上游」
const IDEMPOTENT_REPLAYED_HEADER: &str = "idempotent-replayed";
/// 幂等键长度上限（避免异常长 key 撑爆缓存 key）
const IDEMPOTENCY_KEY_MAX_LEN: usize = 255;

/// 幂等检查结果
enum Idempotency {
    /// 未提供 `Idempotency-Key`：按普通请求处理，行为与旧版本一致
    Disabled,
    /// 命中已完成记录，直接回放
    Replay(RecordedResponse),
    /// 首次执行，由调用方负责登记结果
    Fresh(IdempotencyGuard),
}

/// 解析并检查 `Idempotency-Key`
///
/// 作用域 = `(API key, 方法+路径)`，指纹再叠加请求体：
/// 不同租户复用同一个 key 字符串不会互相回放，同键不同体则报 400。
fn check_idempotency(
    state: &AppState,
    api_key: &Option<String>,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Idempotency, ServerError> {
    let Some(key) = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
    else {
        return Ok(Idempotency::Disabled);
    };
    if key.len() > IDEMPOTENCY_KEY_MAX_LEN {
        return Err(ServerError::IdempotencyConflict);
    }

    let scope = format!("{}:POST {path}", api_key.as_deref().unwrap_or(""));
    let fp = idempotency::fingerprint(&scope, body);
    match state.idempotency.begin(&scope, key, fp)? {
        Begin::Fresh(guard) => Ok(Idempotency::Fresh(guard)),
        Begin::Replay(recorded) => Ok(Idempotency::Replay(recorded)),
        Begin::InProgress => Err(ServerError::IdempotencyInProgress),
        Begin::Unreplayable => Err(ServerError::IdempotencyUnreplayable),
    }
}

/// 直接回放已记录的响应（字节级一致，含状态码与 Content-Type）
fn replay_response(recorded: RecordedResponse) -> Response {
    (
        StatusCode::from_u16(recorded.status).unwrap_or(StatusCode::OK),
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(recorded.content_type),
            ),
            (
                HeaderName::from_static(IDEMPOTENT_REPLAYED_HEADER),
                HeaderValue::from_static("true"),
            ),
        ],
        Body::from(recorded.body),
    )
        .into_response()
}

/// 序列化 JSON 响应体：失败返回 500，而不是在请求路径上 panic
fn json_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ServerError> {
    serde_json::to_vec(value).map_err(|e| {
        ServerError::Adapter(OpenAIAdapterError::Internal(format!("序列化响应失败: {e}")))
    })
}

/// 命中回放时提前返回；否则返回执行守卫（未带 key 时为 `None`）
macro_rules! idempotency_gate {
    ($state:expr, $api_key:expr, $path:expr, $headers:expr, $body:expr) => {
        match check_idempotency($state, $api_key, $path, $headers, $body)? {
            Idempotency::Replay(recorded) => return Ok(replay_response(recorded)),
            Idempotency::Fresh(guard) => Some(guard),
            Idempotency::Disabled => None,
        }
    };
}

/// 用记录器包裹 SSE 流：流正常结束时落库，客户端中途断开则撤销占位
fn record_stream<S, E>(
    stream: S,
    guard: Option<IdempotencyGuard>,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: 'static,
{
    match guard {
        Some(guard) => Box::pin(RecordingStream::new(
            stream,
            guard.into_stream_recorder(StatusCode::OK.as_u16(), "text/event-stream"),
        )),
        None => Box::pin(stream),
    }
}

/// POST /v1/chat/completions
pub(crate) async fn chat_completions(
    State(state): State<AppState>,
    ApiKey(api_key): ApiKey,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ServerError> {
    let request_id = next_request_id();
    let guard = idempotency_gate!(&state, &api_key, "/v1/chat/completions", &headers, &body);
    let timer = super::stats::RequestTimer::new(&state.stats);
    let timer_start = std::time::Instant::now();
    let req: ChatCompletionsRequest = serde_json::from_slice(&body)
        .map_err(|e| OpenAIAdapterError::BadRequest(format!("invalid JSON body: {e}")))?;
    log::debug!(target: "http::request", "req={} POST /v1/chat/completions stream={}", request_id, req.stream);
    let model = req.model.clone();

    let result = state.adapter.chat_completions(req, &request_id).await;
    match &result {
        Ok(_) => timer.mark_success(),
        Err(_) => timer.mark_failure(),
    }
    let result = result?;
    match result.data {
        ChatOutput::Stream(stream) => {
            let prompt_tokens = u64::from(result.prompt_tokens);
            let completion_tokens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let ct_ref = completion_tokens.clone();
            let elapsed = timer_start.elapsed();
            let latency_ms = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            let sse = stream
                .inspect(move |chunk| {
                    if let Ok(c) = chunk
                        && let Some(u) = &c.usage
                    {
                        ct_ref.store(
                            u64::from(u.completion_tokens),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                })
                .map(|chunk| match chunk {
                    Ok(c) => crate::openai_adapter::response::sse_serialize(&c),
                    Err(e) => Err(e),
                });
            let guarded = TokenGuardStream {
                inner: sse,
                _guard: TokenGuard {
                    stats: state.stats.clone(),
                    prompt_tokens,
                    completion_tokens,
                    model: model.clone(),
                    api_key: api_key.clone(),
                    request_id: request_id.clone(),
                    latency_ms,
                    success: true,
                },
            };
            log::debug!(target: "http::response", "req={request_id} 200 SSE stream started");
            let stream = record_stream(guarded, guard);
            Ok(SseBody::new(stream)
                .with_header(X_DS_ACCOUNT, &mask_account_id(&result.account_id))
                .into_response())
        }
        ChatOutput::Json(json) => {
            let pt = u64::from(result.prompt_tokens);
            let ct = json
                .usage
                .as_ref()
                .map_or(0, |u| u64::from(u.completion_tokens));
            let elapsed = timer_start.elapsed();
            let latency_ms = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            state.record_request(&RequestRecord {
                request_id: &request_id,
                model: &model,
                api_key: &api_key,
                prompt_tokens: pt,
                completion_tokens: ct,
                latency_ms,
                success: true,
            });
            let bytes = json_bytes(&json)?;
            if let Some(guard) = guard {
                guard.complete(
                    StatusCode::OK.as_u16(),
                    "application/json",
                    Bytes::from(bytes.clone()),
                );
            }
            log::debug!(target: "http::response", "req={} 200 JSON response {} bytes", request_id, bytes.len());
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(X_DS_ACCOUNT, &mask_account_id(&result.account_id))
                .body(Body::from(bytes))
                .unwrap()
                .into_response())
        }
    }
}

/// GET /v1/responses/{id}
///
/// 检索此前创建并保存的 Response 对象（issue #110）。
/// 快照保存在进程内有界 + TTL 缓存中：不存在 / 已被淘汰 / 已过期 → 404。
pub(crate) async fn responses_get(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ServerError> {
    log::debug!(target: "http::request", "GET /v1/responses/{id}");
    state.responses_adapter.get_response(&id).map_or_else(
        || Err(ServerError::NotFound(format!("response '{id}'"))),
        |snapshot| {
            let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
            log::debug!(target: "http::response", "200 JSON response {} bytes", bytes.len());
            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                Body::from(bytes),
            )
                .into_response())
        },
    )
}

/// POST /v1/responses
///
/// OpenAI Responses API。流式返回 `text/event-stream`（`event: <type>` +
/// `data: <json>`），非流式返回 Response 对象。
pub(crate) async fn responses(
    State(state): State<AppState>,
    ApiKey(api_key): ApiKey,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ServerError> {
    let request_id = next_request_id();
    let guard = idempotency_gate!(&state, &api_key, "/v1/responses", &headers, &body);
    let timer = super::stats::RequestTimer::new(&state.stats);
    let timer_start = std::time::Instant::now();
    let req: ResponsesRequest = serde_json::from_slice(&body)
        .map_err(|e| OpenAIAdapterError::BadRequest(format!("invalid JSON body: {e}")))?;
    log::debug!(
        target: "http::request",
        "req={} POST /v1/responses stream={}", request_id, req.stream
    );
    let model = req.model.clone();

    let result = state.responses_adapter.create(req, &request_id).await;
    match &result {
        Ok(_) => timer.mark_success(),
        Err(_) => timer.mark_failure(),
    }
    let result = result?;

    match result.data {
        ResponsesOutput::Stream(stream) => {
            let prompt_tokens = u64::from(result.prompt_tokens);
            let completion_tokens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let ct_ref = completion_tokens.clone();
            // Responses 的 usage 落在 `response.completed` 事件里，从 SSE 文本中提取
            let sse = stream.inspect(move |chunk| {
                if let Ok(bytes) = chunk
                    && let Ok(text) = std::str::from_utf8(bytes)
                    && text.contains("\"output_tokens\"")
                    && let Some(ct) = extract_output_tokens(text)
                {
                    ct_ref.store(ct, std::sync::atomic::Ordering::Relaxed);
                }
            });
            let elapsed = timer_start.elapsed();
            let latency_ms = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            let guarded = TokenGuardStream {
                inner: sse,
                _guard: TokenGuard {
                    stats: state.stats.clone(),
                    prompt_tokens,
                    completion_tokens,
                    model: model.clone(),
                    api_key: api_key.clone(),
                    request_id: request_id.clone(),
                    latency_ms,
                    success: true,
                },
            };
            log::debug!(target: "http::response", "req={request_id} 200 Responses SSE started");
            let stream = record_stream(guarded, guard);
            Ok(SseBody::new(stream)
                .with_header(X_DS_ACCOUNT, &mask_account_id(&result.account_id))
                .with_header("openai-processing-ms", &latency_ms.to_string())
                .into_response())
        }
        ResponsesOutput::Json(json) => {
            let pt = u64::from(result.prompt_tokens);
            let ct = u64::from(json.usage.as_ref().map_or(0, |u| u.output_tokens));
            let elapsed = timer_start.elapsed();
            let latency_ms = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            state.record_request(&RequestRecord {
                request_id: &request_id,
                model: &model,
                api_key: &api_key,
                prompt_tokens: pt,
                completion_tokens: ct,
                latency_ms,
                success: true,
            });
            let bytes = json_bytes(&json)?;
            if let Some(guard) = guard {
                guard.complete(
                    StatusCode::OK.as_u16(),
                    "application/json",
                    Bytes::from(bytes.clone()),
                );
            }
            log::debug!(
                target: "http::response",
                "req={} 200 Responses JSON {} bytes", request_id, bytes.len()
            );
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(X_DS_ACCOUNT, &mask_account_id(&result.account_id))
                .header("openai-processing-ms", latency_ms.to_string())
                .body(Body::from(bytes))
                .unwrap()
                .into_response())
        }
    }
}

/// 从 `response.completed` 的 SSE 文本中提取 `output_tokens`（仅用于统计）
fn extract_output_tokens(text: &str) -> Option<u64> {
    let key = "\"output_tokens\":";
    let start = text.find(key)? + key.len();
    let rest = text[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// GET /v1/models
pub(crate) async fn list_models(State(state): State<AppState>) -> Response {
    log::debug!(target: "http::request", "GET /v1/models");
    let bytes = serde_json::to_vec(&state.adapter.list_models().await).unwrap();
    log::debug!(target: "http::response", "200 JSON response {} bytes", bytes.len());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(bytes),
    )
        .into_response()
}

/// GET /v1/models/{id}
pub(crate) async fn get_model(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ServerError> {
    log::debug!(target: "http::request", "GET /v1/models/{id}");

    state.adapter.get_model(&id).await.map_or_else(
        || Err(ServerError::NotFound(id)),
        |model| {
            let bytes = serde_json::to_vec(&model).unwrap();
            log::debug!(target: "http::response", "200 JSON response {} bytes", bytes.len());
            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                Body::from(bytes),
            )
                .into_response())
        },
    )
}

// ============================================================================
// Anthropic 兼容路由
// ============================================================================

/// POST /anthropic/v1/messages
pub(crate) async fn anthropic_messages(
    State(state): State<AppState>,
    ApiKey(api_key): ApiKey,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ServerError> {
    let request_id = next_request_id();
    let timer = super::stats::RequestTimer::new(&state.stats);
    let timer_start = std::time::Instant::now();

    let guard = idempotency_gate!(&state, &api_key, "/anthropic/v1/messages", &headers, &body);
    let req: MessagesRequest = serde_json::from_slice(&body)
        .map_err(|e| AnthropicCompatError::BadRequest(format!("invalid JSON body: {e}")))?;
    log::debug!(target: "http::request", "req={} POST /anthropic/v1/messages stream={}", request_id, req.stream);
    let model = req.model.clone();

    let result = state.anthropic_compat.messages(req, &request_id).await;
    match &result {
        Ok(_) => timer.mark_success(),
        Err(_) => timer.mark_failure(),
    }
    let result = result?;
    match result.data {
        AnthropicOutput::Stream(stream) => {
            let prompt_tokens = u64::from(result.prompt_tokens);
            let stats = state.stats.clone();
            let completion_tokens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let ct_ref = completion_tokens.clone();
            let sse = stream
                .inspect(move |chunk| {
                    if let Ok(c) = chunk
                        && let Some(ot) = c.output_tokens()
                    {
                        // Anthropic 的 `message_delta.usage.output_tokens` 是
                        // **累计**值（MessageDeltaUsage 的官方描述为
                        // "cumulative number of output tokens"），不是增量。
                        // 用 fetch_add 会在事件重复时重复计数，必须整体覆盖。
                        ct_ref.store(u64::from(ot), std::sync::atomic::Ordering::Relaxed);
                    }
                })
                .map(|chunk| match chunk {
                    Ok(c) => c
                        .to_sse_bytes()
                        .map_err(|e| AnthropicCompatError::Internal(e.to_string())),
                    Err(e) => Err(e),
                });
            // Attach guard as a stream wrapper so it drops when the stream is consumed/dropped
            let elapsed = timer_start.elapsed();
            let latency = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            let guarded = TokenGuardStream {
                inner: sse,
                _guard: TokenGuard {
                    stats,
                    prompt_tokens,
                    completion_tokens,
                    model: model.clone(),
                    api_key: api_key.clone(),
                    request_id: request_id.clone(),
                    latency_ms: latency,
                    success: true,
                },
            };
            log::debug!(target: "http::response", "req={request_id} 200 SSE stream started");
            let stream = record_stream(guarded, guard);
            Ok(SseBody::new(stream)
                .with_header(X_DS_ACCOUNT, &mask_account_id(&result.account_id))
                .with_header(ANTHROPIC_REQUEST_ID, &request_id)
                .into_response())
        }
        AnthropicOutput::Json(json) => {
            let pt = u64::from(result.prompt_tokens);
            let ct = u64::from(json.usage.output_tokens);
            let elapsed = timer_start.elapsed();
            let latency_ms = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            state.record_request(&RequestRecord {
                request_id: &request_id,
                model: &model,
                api_key: &api_key,
                prompt_tokens: pt,
                completion_tokens: ct,
                latency_ms,
                success: true,
            });
            let bytes = json_bytes(&json)?;
            if let Some(guard) = guard {
                guard.complete(
                    StatusCode::OK.as_u16(),
                    "application/json",
                    Bytes::from(bytes.clone()),
                );
            }
            log::debug!(target: "http::response", "req={} 200 JSON response {} bytes", request_id, bytes.len());
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(X_DS_ACCOUNT, &mask_account_id(&result.account_id))
                .header(ANTHROPIC_REQUEST_ID, &request_id)
                .body(Body::from(bytes))
                .unwrap()
                .into_response())
        }
    }
}

/// GET /anthropic/v1/models
pub(crate) async fn anthropic_list_models(State(state): State<AppState>) -> Response {
    log::debug!(target: "http::request", "GET /anthropic/v1/models");
    let bytes = serde_json::to_vec(&state.anthropic_compat.list_models().await).unwrap();
    log::debug!(target: "http::response", "200 JSON response {} bytes", bytes.len());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(bytes),
    )
        .into_response()
}

/// GET /anthropic/v1/models/{id}
pub(crate) async fn anthropic_get_model(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ServerError> {
    log::debug!(target: "http::request", "GET /anthropic/v1/models/{id}");

    state.anthropic_compat.get_model(&id).await.map_or_else(
        // Anthropic 客户端只识别 Anthropic 形态的错误信封
        || {
            Ok(super::error::anthropic_not_found_error(&format!(
                "model '{id}' not found"
            )))
        },
        |model| {
            let bytes = serde_json::to_vec(&model).unwrap();
            log::debug!(target: "http::response", "200 JSON response {} bytes", bytes.len());
            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                Body::from(bytes),
            )
                .into_response())
        },
    )
}

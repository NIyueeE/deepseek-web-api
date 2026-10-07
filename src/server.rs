//! HTTP 服务器层 —— 薄路由壳，暴露 OpenAIAdapter 与 AnthropicCompat 为 HTTP 接口
//!
//! 本模块负责将 adapter / compat 层包装为 axum HTTP 服务。

mod admin;
mod auth;
mod error;
mod handlers;
mod idempotency;
pub mod runtime_log;
mod stats;
mod store;
mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::{
    Json, Router,
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

use crate::anthropic_compat::AnthropicCompat;
use crate::config::Config;
use crate::openai_adapter::OpenAIAdapter;
use crate::responses_adapter::ResponsesAdapter;

use handlers::AppState;

/// Extension to carry the API key through the request
#[derive(Clone)]
pub(crate) struct ApiKeyExt(pub(crate) String);

/// 脱敏字符串前缀：超过 `keep` 个字符时保留前 `keep` 个字符并追加 `***`，否则整体打码
///
/// 使用 `chars()` 而非字节切片：`api_keys` 未约束为 ASCII，直接 `&s[..keep]`
/// 在含中文 / emoji 的 key 上会切到 UTF-8 续字节而 panic。
pub(crate) fn mask_prefix(s: &str, keep: usize) -> String {
    if s.chars().count() > keep {
        let prefix: String = s.chars().take(keep).collect();
        format!("{prefix}***")
    } else {
        "***".to_string()
    }
}

/// 启动 HTTP 服务器
pub async fn run(config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    let cors_origins = config.server.cors_origins.clone();
    let host = config.server.host.clone();
    let port = config.server.port;
    let responses_store_capacity = config.ds_core.responses_store_capacity;
    let responses_store_ttl_secs = config.ds_core.responses_store_ttl_secs;
    let adapter = Arc::new(OpenAIAdapter::new(&config).await?);
    let config = Arc::new(tokio::sync::RwLock::new(config));
    let anthropic_compat = Arc::new(AnthropicCompat::new(Arc::clone(&adapter)));
    let responses_adapter = Arc::new(ResponsesAdapter::new(
        Arc::clone(&adapter),
        responses_store_capacity,
        responses_store_ttl_secs,
    ));
    let data_dir = std::env::var("DS_DATA_DIR").unwrap_or_else(|_| ".".to_string());
    let store = Arc::new(store::StoreManager::new(
        std::path::Path::new(&data_dir),
        &config_path,
        config.clone(),
    ));
    let stats = Arc::new(stats::Stats::new_with_store(Some(store.clone())));
    let login_limiter = Arc::new(auth::LoginLimiter::new());
    let state = AppState {
        adapter: adapter.clone(),
        anthropic_compat,
        responses_adapter,
        stats: stats.clone(),
        config: config.clone(),
        config_path: config_path.clone(),
        store: store.clone(),
        login_limiter: login_limiter.clone(),
        idempotency: Arc::new(idempotency::IdempotencyStore::new()),
    };
    let router = build_router(state.clone(), &cors_origins);

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    log::info!(target: "http::server", "openai-compatible base_url: http://{addr}");
    log::info!(target: "http::server", "responses-compatible base_url: http://{addr}");
    log::info!(target: "http::server", "anthropic-compatible base_url: http://{addr}");
    log::info!(target: "http::server", "admin panel: http://{addr}/admin");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    log::info!(target: "http::server", "HTTP server stopped, cleaning up resources");
    stats.persist_now();
    state.adapter.shutdown().await;
    log::info!(target: "http::server", "cleanup complete");

    Ok(())
}

/// 构建路由器
fn build_router(state: AppState, cors_origins: &[String]) -> Router {
    let store = state.store.clone();

    let public = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        // Admin auth (no JWT required)
        .route("/admin/api/setup", post(admin::admin_setup))
        .route("/admin/api/login", post(admin::admin_login));

    // API routes: Bearer token from api_keys.json (OpenAI 形态错误信封)
    let openai_routes = Router::new()
        // OpenAI
        .route("/v1/chat/completions", post(handlers::chat_completions))
        // OpenAI Responses API
        .route("/v1/responses", post(handlers::responses))
        .route("/v1/responses/{id}", get(handlers::responses_get))
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/models/{id}", get(handlers::get_model))
        .layer(middleware::from_fn(move |req, next| {
            let store = store.clone();
            async move { api_key_middleware(req, next, store, ErrorFlavor::OpenAi).await }
        }));

    // Anthropic 路由：独立鉴权中间件，错误信封为 Anthropic 形态，
    // 且额外接受 `x-api-key` 头（Anthropic SDK / Claude Code 的默认鉴权方式）。
    let anthropic_store = state.store.clone();
    let anthropic_routes = Router::new()
        .route("/anthropic/v1/messages", post(handlers::anthropic_messages))
        .route("/anthropic/v1/models", get(handlers::anthropic_list_models))
        .route(
            "/anthropic/v1/models/{id}",
            get(handlers::anthropic_get_model),
        )
        .layer(middleware::from_fn(move |req, next| {
            let store = anthropic_store.clone();
            async move { api_key_middleware(req, next, store, ErrorFlavor::Anthropic).await }
        }));

    // Admin routes: JWT auth
    let admin_store = state.store.clone();
    let admin_routes = Router::new()
        .route("/admin/api/status", get(admin::admin_status))
        .route(
            "/admin/api/account-statuses-detailed",
            get(admin::admin_account_statuses_detailed),
        )
        .route("/admin/api/sessions", get(admin::admin_sessions))
        .route("/admin/api/stats", get(admin::admin_stats))
        .route("/admin/api/models", get(admin::admin_models))
        .route("/admin/api/config", get(admin::admin_config))
        // Config
        .route("/admin/api/config", put(admin::admin_put_config))
        // Request logs
        .route("/admin/api/logs", get(admin::admin_logs))
        // Runtime logs
        .route("/admin/api/runtime-logs", get(admin::admin_runtime_logs))
        .layer(middleware::from_fn(move |req, next| {
            let store = admin_store.clone();
            async move { jwt_middleware(req, next, store).await }
        }));

    let router = public
        .merge(openai_routes)
        .merge(anthropic_routes)
        .merge(admin_routes);

    // 静态文件服务：/admin → web/dist/
    // 优先从文件系统读取（开发模式），回退到编译时嵌入的资源（release 二进制）
    let web_dist = std::path::Path::new("web/dist");
    let router = if web_dist.exists() {
        router.nest_service(
            "/admin",
            tower_http::services::ServeDir::new(web_dist)
                .fallback(tower_http::services::ServeFile::new("web/dist/index.html")),
        )
    } else {
        // 编译时嵌入：fallback 模式，不注册具体路由，无冲突风险
        router.fallback(serve_embedded_fallback)
    };

    router
        .with_state(state)
        .layer(DefaultBodyLimit::max(10_000_000))
        .layer(build_cors_layer(cors_origins))
        // 最外层：所有响应（含鉴权失败）都带 `x-request-id`，与日志里的 `req=` 一致
        .layer(middleware::from_fn(request_id_middleware))
}

/// 为每个请求生成 `req-{n}`：写入扩展供 handler 复用，并回填 `x-request-id` 响应头
async fn request_id_middleware(mut req: Request, next: Next) -> Response {
    let id = handlers::next_request_id();
    if let Ok(value) = HeaderValue::from_str(&id) {
        req.extensions_mut().insert(handlers::RequestId(id.clone()));
        let mut resp = next.run(req).await;
        resp.headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
        return resp;
    }
    next.run(req).await
}

/// 构建 CORS 层
///
/// `cors_origins` 的语义：
/// - `["*"]` → 完全放开（`permissive`）
/// - 其余 → 严格白名单
///
/// **失败安全**：若配置了白名单但其中没有任何一项能解析成合法 `HeaderValue`
/// （典型是配错成 `"localhost:22217"` 这种缺少 scheme 的写法），旧实现会静默回退到
/// `permissive` —— 用户以为自己限制了来源，实际完全放开。现在改为回退到
/// **拒绝所有跨域来源** 并打出警告，让配置错误显式暴露。
fn build_cors_layer(origins: &[String]) -> CorsLayer {
    use axum::http::Method;
    use axum::http::header;

    if origins.len() == 1 && origins[0] == "*" {
        return CorsLayer::permissive();
    }

    let mut allowed: Vec<axum::http::HeaderValue> = Vec::with_capacity(origins.len());
    for origin in origins {
        match origin.parse::<axum::http::HeaderValue>() {
            Ok(value) => allowed.push(value),
            Err(e) => {
                log::warn!(
                    target: "http::server",
                    "cors_origins 中的 {origin:?} 不是合法的 Origin（需要包含 scheme，如 http://localhost:22217）：{e}"
                );
            }
        }
    }

    if allowed.is_empty() {
        log::error!(
            target: "http::server",
            "cors_origins 未解析出任何合法 Origin，已禁用跨域访问（如需放开请显式设置 cors_origins = [\"*\"]）"
        );
        // 空 allow_origin 列表 = 不放行任何跨域来源（fail closed）
        return CorsLayer::new()
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([header::CONTENT_TYPE]);
    }

    CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            // 浏览器端 Anthropic 客户端会带这两个头；缺失会被 preflight 拒绝
            axum::http::HeaderName::from_static("x-api-key"),
            axum::http::HeaderName::from_static("anthropic-version"),
            axum::http::HeaderName::from_static("x-request-id"),
        ])
}

/// 编译时嵌入 web/dist/ 目录，release 二进制无需额外文件即可提供管理面板
#[derive(rust_embed::Embed)]
#[folder = "web/dist/"]
struct WebAssets;

/// 编译时嵌入资源 fallback：仅处理 /admin 及 /admin/* 路径，其余返回 404
async fn serve_embedded_fallback(uri: axum::http::Uri) -> Response {
    use axum::http::{StatusCode, header};

    let path = uri.path();
    if path == "/admin" || path.starts_with("/admin/") {
        let key = path
            .strip_prefix("/admin/")
            .unwrap_or("")
            .trim_start_matches('/');
        if !key.is_empty()
            && let Some(content) = WebAssets::get(key)
        {
            let mime = mime_guess::from_path(key).first_or_octet_stream();
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref())],
                content.data,
            )
                .into_response();
        }
        // SPA fallback
        if let Some(content) = WebAssets::get("index.html") {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                content.data,
            )
                .into_response();
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn root() -> axum::response::Redirect {
    axum::response::Redirect::to("/admin")
}

/// Health check endpoint
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok"
    }))
}

/// 错误信封形态：中间件按路由前缀选择，保证客户端收到规范格式
#[derive(Clone, Copy, PartialEq, Eq)]
enum ErrorFlavor {
    OpenAi,
    Anthropic,
}

/// API Key 鉴权中间件（从 config.toml 的 api_keys 校验）
///
/// 支持两种凭据头：
/// - `Authorization: Bearer <key>`（OpenAI SDK / Anthropic SDK 的 `authToken`）
/// - `x-api-key: <key>`（Anthropic SDK / Claude Code 的默认方式）
async fn api_key_middleware(
    req: Request,
    next: Next,
    store: Arc<store::StoreManager>,
    flavor: ErrorFlavor,
) -> Response {
    let token = extract_api_token(&req);
    let valid = match token {
        Some(t) => store.is_valid_api_key(t).await,
        None => false,
    };

    if !valid {
        log::debug!(target: "http::response", "401 unauthorized API request");
        return match flavor {
            ErrorFlavor::OpenAi => error::ServerError::Unauthorized.into_response(),
            ErrorFlavor::Anthropic => error::anthropic_auth_error(),
        };
    }

    // Inject the API key into request extensions for downstream handlers
    let key_ext = token.map(|t| ApiKeyExt(t.to_string()));
    let mut req = req;
    if let Some(ext) = key_ext {
        req.extensions_mut().insert(ext);
    }

    next.run(req).await
}

/// JWT 鉴权中间件（管理面板路由）
async fn jwt_middleware(req: Request, next: Next, store: Arc<store::StoreManager>) -> Response {
    let token = extract_bearer_token(&req);
    let valid = match token {
        Some(t) => auth::verify_jwt(&store, t).await,
        None => false,
    };

    if !valid {
        log::debug!(target: "http::response", "401 unauthorized admin request");
        return error::ServerError::Unauthorized.into_response();
    }

    next.run(req).await
}

/// 从 Authorization 头提取 Bearer token
fn extract_bearer_token(req: &Request) -> Option<&str> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
}

/// 提取 API 凭据：优先 `Authorization: Bearer`，其次 `x-api-key`
///
/// Anthropic 官方 SDK 默认发 `x-api-key`（见 anthropic-sdk-typescript
/// `src/client.ts` 的 `apiKey` 分支），仅支持 Bearer 会让 Claude Code 无法鉴权。
fn extract_api_token(req: &Request) -> Option<&str> {
    extract_bearer_token(req).or_else(|| {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
    })
}

/// 优雅关闭信号
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    log::info!(target: "http::server", "shutdown signal received, starting graceful shutdown");
}

// ============================================================================
// HTTP 层测试：鉴权中间件、错误信封、路由挂载
// ============================================================================

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;

    /// 构造一个带 API key 的测试配置
    fn test_config(key: &str) -> Config {
        let toml_str = format!(
            r#"
[server]
host = "127.0.0.1"
port = 22217

[ds_core]

[[api_keys]]
key = "{key}"
description = "test"
"#
        );
        toml::from_str(&toml_str).expect("test config must parse")
    }

    /// 构造一个仅用于测试的 store（stats.json 落在临时目录）
    fn test_store(key: &str) -> Arc<store::StoreManager> {
        let dir = std::env::temp_dir().join(format!(
            "ds-free-api-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let config = Arc::new(tokio::sync::RwLock::new(test_config(key)));
        Arc::new(store::StoreManager::new(&dir, &config_path, config))
    }

    /// 带 `x-request-id` 中间件的桩路由（与 `build_router` 的最外层一致）
    fn request_id_router() -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .layer(middleware::from_fn(request_id_middleware))
    }

    /// 每个响应都必须带 `x-request-id`，且与日志里的 `req=` 同源
    #[tokio::test]
    async fn responses_carry_x_request_id() {
        let resp = request_id_router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let id = resp
            .headers()
            .get("x-request-id")
            .expect("缺少 x-request-id")
            .to_str()
            .unwrap();
        assert!(id.starts_with("req-"), "意外的请求 ID: {id}");
    }

    /// 与 `build_router` 相同的中间件挂载方式，但 handler 用桩函数替代
    /// （真实 handler 需要账号池，单元测试不应触网）
    fn auth_router(store: Arc<store::StoreManager>) -> Router {
        let openai_store = store.clone();
        let openai = Router::new()
            .route("/v1/models", get(|| async { "openai-ok" }))
            .layer(middleware::from_fn(move |req, next| {
                let store = openai_store.clone();
                async move { api_key_middleware(req, next, store, ErrorFlavor::OpenAi).await }
            }));

        let anthropic_store = store;
        let anthropic = Router::new()
            .route("/anthropic/v1/messages", post(|| async { "anthropic-ok" }))
            .route(
                "/anthropic/v1/models/{id}",
                get(|| async { "anthropic-model-ok" }),
            )
            .layer(middleware::from_fn(move |req, next| {
                let store = anthropic_store.clone();
                async move { api_key_middleware(req, next, store, ErrorFlavor::Anthropic).await }
            }));

        openai.merge(anthropic)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn openai_route_requires_bearer_token() {
        let router = auth_router(test_store("sk-test"));
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(resp).await;
        // OpenAI 错误信封：type / message / param / code 四字段齐备
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert!(body["error"]["message"].is_string());
        assert!(body["error"].get("param").is_some());
    }

    #[tokio::test]
    async fn openai_route_accepts_valid_bearer_token() {
        let router = auth_router(test_store("sk-test"));
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, "Bearer sk-test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn openai_route_rejects_wrong_token() {
        let router = auth_router(test_store("sk-test"));
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, "Bearer sk-wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Claude Code / Anthropic SDK 默认发 `x-api-key`，中间件必须接受
    #[tokio::test]
    async fn anthropic_route_accepts_x_api_key_header() {
        let router = auth_router(test_store("sk-test"));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anthropic/v1/messages")
                    .header("x-api-key", "sk-test")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn anthropic_route_also_accepts_bearer_token() {
        let router = auth_router(test_store("sk-test"));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anthropic/v1/messages")
                    .header(header::AUTHORIZATION, "Bearer sk-test")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Anthropic 错误信封必须是 `{"type":"error","error":{...}}`
    #[tokio::test]
    async fn anthropic_route_uses_anthropic_error_envelope() {
        let router = auth_router(test_store("sk-test"));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anthropic/v1/messages")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(resp).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert!(body["error"]["message"].is_string());
        // 顶层不应出现 OpenAI 形态的 `code`
        assert!(body.get("error").unwrap().get("code").is_none());
    }

    #[tokio::test]
    async fn anthropic_missing_key_404_uses_anthropic_envelope() {
        // 直接验证 404 信封构造函数（handler 依赖账号池，无法在单测中构造）
        let resp = error::anthropic_not_found_error("model 'x' not found");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "not_found_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn extract_api_token_prefers_bearer_then_x_api_key() {
        let bearer = Request::builder()
            .header(header::AUTHORIZATION, "Bearer from-bearer")
            .header("x-api-key", "from-x-api-key")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_token(&bearer), Some("from-bearer"));

        let xkey = Request::builder()
            .header("x-api-key", "from-x-api-key")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_token(&xkey), Some("from-x-api-key"));

        let empty = Request::builder()
            .header("x-api-key", "")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_token(&empty), None);

        let none = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(extract_api_token(&none), None);
    }

    /// 脱敏必须按字符截断：`api_keys` 未限制为 ASCII，字节切片会 panic
    #[test]
    fn mask_prefix_handles_non_ascii() {
        assert_eq!(mask_prefix("sk-abcdefghij", 8), "sk-abcde***");
        assert_eq!(mask_prefix("sk-short", 8), "***");
        assert_eq!(
            mask_prefix("密钥密钥密钥密钥密钥", 8),
            "密钥密钥密钥密钥***"
        );
        assert_eq!(mask_prefix("🔑🔑🔑", 2), "🔑🔑***");
    }

    #[tokio::test]
    async fn strip_prefix_is_case_sensitive_like_openai() {
        // `bearer` 小写在 OpenAI SDK 中不使用；保持严格前缀匹配
        let req = Request::builder()
            .header(header::AUTHORIZATION, "bearer sk-test")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_bearer_token(&req), None);
    }

    #[tokio::test]
    async fn cors_layer_permissive_on_wildcard() {
        let layer = build_cors_layer(&["*".to_string()]);
        // `permissive()` 会应答 preflight；这里只断言构造不 panic 且可挂载
        let router = Router::new()
            .route("/health", get(|| async { "ok" }))
            .layer(layer);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "http://example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
    }

    #[tokio::test]
    async fn cors_layer_restricted_origin_only_allows_listed() {
        let layer = build_cors_layer(&["http://allowed.example".to_string()]);
        let router = Router::new()
            .route("/health", get(|| async { "ok" }))
            .layer(layer);

        let allowed = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "http://allowed.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            allowed
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://allowed.example")
        );

        let denied = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::ORIGIN, "http://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            denied
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "未在 cors_origins 中的 Origin 不得获得 CORS 许可"
        );
    }
}

//! HIF 风控令牌 —— 对齐真实客户端的 `x-hif-leim` + `x-hif-dliq`
//!
//! 真实 Web 客户端启动后会轮询 `https://hif-leim.deepseek.com/query`（**无鉴权**），
//! 把响应 `data.biz_data.value` 缓存进 localStorage（`hif_leim_cached`），
//! 有效期取响应头 `x-hif-ttl`（默认 600s），并在 **completion（SSE）请求**上以
//! `x-hif-leim` 头回传（前端源码里该头部由 `addSSEHeader` 注入）。
//!
//! 也就是说：请求 `/chat/completion` 时缺少该头，上游可以直接判定请求并非来自
//! 官方客户端 —— 这是第三方反代最容易漏掉、也最难猜到的风控信号。
//!
//! 2026-10-07 补充取证（前端 `main.*.js`）：真实客户端其实有**两个**同构的轮询器
//! （`leimPoller` / `dliqPoller`），分别写 `hif_leim_cached` / `hif_dliq_cached`，
//! 并由同一个头提供者**一起**附加：
//!
//! ```js
//! (n = headers.leim || store.leim.get() || "") && (out["x-hif-leim"] = n);
//! (r = headers.dliq || store.dliq.get() || "") && (out["x-hif-dliq"] = r);
//! ```
//!
//! 也就是说「只发 `x-hif-leim`」在 `hif-dliq` 可解析的网络里仍然少一个头。
//! 本模块因此按**设备 → 两个令牌**分桶：两个端点各自缓存、各自退避，
//! 谁取到就带谁（取不到就跳过，与客户端 `&&` 的行为一致）。
//!
//! 取令牌失败时不会阻断业务请求：与真实客户端轮询失败时的行为一致，
//! 本次请求不带该头，并按退避稍后重试。

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use log::{debug, warn};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::client::ClientError;

/// 上游未返回 `x-hif-ttl` 时的默认有效期
const DEFAULT_TTL_SECS: u64 = 600;
/// 单次取令牌的超时（真实客户端为 3s）
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// 到期前提前刷新，避免在边界上发出即将失效的令牌
const REFRESH_MARGIN: Duration = Duration::from_secs(30);
/// 取令牌失败后的首次退避：避免每个请求都白等一次 3s 超时
///
/// 真实客户端的轮询器是独立定时器（1s 起、上限 `hif_max_retry_interval_secs` 默认 600s）；
/// 我们的取值发生在请求路径上，因此起始退避取得更保守（30s），上限与客户端一致。
const INITIAL_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// 取令牌失败退避上限（对齐 `hif_max_retry_interval_secs` 默认值）
const MAX_FAILURE_BACKOFF: Duration = Duration::from_mins(10);

/// 两个同构的风控令牌端点
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HifKind {
    Leim,
    Dliq,
}

impl HifKind {
    /// 随 SSE 请求下发的请求头名（与官方客户端一致）
    pub(crate) const fn header(self) -> &'static str {
        match self {
            Self::Leim => "X-Hif-Leim",
            Self::Dliq => "X-Hif-Dliq",
        }
    }

    /// 日志/诊断名
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Leim => "x-hif-leim",
            Self::Dliq => "x-hif-dliq",
        }
    }

    /// 官方 localStorage 缓存键（对照抓包用）
    pub(crate) const fn cache_key(self) -> &'static str {
        match self {
            Self::Leim => "hif_leim_cached",
            Self::Dliq => "hif_dliq_cached",
        }
    }

    const ALL: [Self; 2] = [Self::Leim, Self::Dliq];
}

#[derive(Debug, Deserialize)]
struct HifEnvelope {
    data: Option<HifData>,
}

#[derive(Debug, Deserialize)]
struct HifData {
    biz_code: i64,
    #[serde(default)]
    biz_msg: String,
    biz_data: Option<HifValue>,
}

#[derive(Debug, Deserialize)]
struct HifValue {
    value: String,
}

/// 解析 `{"code":0,"data":{"biz_code":0,"biz_data":{"value":"..."}}}` 信封
fn parse_value(body: &str) -> Result<String, ClientError> {
    let env: HifEnvelope = serde_json::from_str(body)?;
    let data = env.data.ok_or_else(|| ClientError::Business {
        code: -1,
        msg: "missing data".into(),
    })?;
    if data.biz_code != 0 {
        return Err(ClientError::Business {
            code: data.biz_code,
            msg: data.biz_msg,
        });
    }
    let value = data
        .biz_data
        .ok_or_else(|| ClientError::Business {
            code: -1,
            msg: "missing biz_data".into(),
        })?
        .value;
    if value.is_empty() {
        return Err(ClientError::Business {
            code: -1,
            msg: "empty hif value".into(),
        });
    }
    Ok(value)
}

struct Cached {
    value: String,
    expires_at: Instant,
}

#[derive(Default)]
struct State {
    cached: Option<Cached>,
    /// 失败退避截止时间（此时间段内不再尝试取新令牌）
    next_attempt_at: Option<Instant>,
    /// 连续失败次数（用于指数退避）
    failures: u32,
}

/// 单个 HIF 令牌端点（leim / dliq）的取值与缓存
///
/// 令牌与「设备 + 出口 IP」绑定，因此**按设备（X-Device-Id）分别缓存**：
/// 真实客户端一个浏览器 profile 一个设备身份，代理侧同理按账号派生设备身份，
/// 多账号共用同一个令牌会把它们关联成同一台设备。
pub(crate) struct HifToken {
    kind: HifKind,
    http: wreq::Client,
    url: String,
    /// 客户端拟态头（与业务请求一致，但不带 Authorization）
    headers: wreq::header::HeaderMap,
    state: Mutex<State>,
}

/// 某个设备身份下的两个令牌句柄
pub(crate) struct DeviceTokens {
    leim: Arc<HifToken>,
    dliq: Arc<HifToken>,
}

impl DeviceTokens {
    const fn get(&self, kind: HifKind) -> &Arc<HifToken> {
        match kind {
            HifKind::Leim => &self.leim,
            HifKind::Dliq => &self.dliq,
        }
    }

    /// 依次取两个令牌；取不到的跳过（与官方客户端 `&&` 的短路行为一致）
    pub(crate) async fn values(&self) -> Vec<(HifKind, String)> {
        let mut out = Vec::with_capacity(2);
        for kind in HifKind::ALL {
            if let Some(value) = self.get(kind).value().await {
                out.push((kind, value));
            }
        }
        out
    }
}

/// 按设备身份分发 HIF 令牌（每个 X-Device-Id 一份独立缓存与刷新周期，
/// 每个设备下再分 leim / dliq 两个端点）
pub(crate) struct HifRegistry {
    http: wreq::Client,
    leim_url: String,
    dliq_url: String,
    /// 除 `X-Device-Id` 之外的客户端拟态头模板
    headers: wreq::header::HeaderMap,
    tokens: DashMap<String, Arc<DeviceTokens>>,
}

impl HifRegistry {
    pub(crate) fn new(
        http: wreq::Client,
        leim_url: String,
        dliq_url: String,
        headers: wreq::header::HeaderMap,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            leim_url,
            dliq_url,
            headers,
            tokens: DashMap::new(),
        })
    }

    /// 取某设备身份的两个令牌句柄（首次调用时创建，之后复用同一份缓存）
    pub(crate) fn tokens_for(&self, device_id: &str) -> Arc<DeviceTokens> {
        if let Some(tokens) = self.tokens.get(device_id) {
            return Arc::clone(tokens.value());
        }
        let mut headers = self.headers.clone();
        if let Ok(value) = wreq::header::HeaderValue::from_str(device_id) {
            headers.insert("X-Device-Id", value);
        }
        let device = Arc::new(DeviceTokens {
            leim: HifToken::new(
                HifKind::Leim,
                self.http.clone(),
                self.leim_url.clone(),
                headers.clone(),
            ),
            dliq: HifToken::new(
                HifKind::Dliq,
                self.http.clone(),
                self.dliq_url.clone(),
                headers,
            ),
        });
        Arc::clone(
            self.tokens
                .entry(device_id.to_string())
                .or_insert(device)
                .value(),
        )
    }

    /// 预热某设备的两个令牌
    pub(crate) async fn warm_up(&self, device_id: &str) {
        let tokens = self.tokens_for(device_id);
        for kind in HifKind::ALL {
            tokens.get(kind).warm_up().await;
        }
    }
}

/// 失败退避：30s 起，指数增长，上限 10 分钟
fn failure_backoff(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(10);
    INITIAL_FAILURE_BACKOFF
        .saturating_mul(1 << exp)
        .min(MAX_FAILURE_BACKOFF)
}

/// 把取到的令牌写进请求头（空值跳过，与官方客户端一致）
pub(crate) fn insert_tokens(
    headers: &mut wreq::header::HeaderMap,
    values: &[(HifKind, String)],
) -> Result<(), ClientError> {
    for (kind, value) in values {
        if value.is_empty() {
            continue;
        }
        debug!(
            target: "ds_core::client",
            "attach {} ({} chars)", kind.label(), value.len()
        );
        headers.insert(
            kind.header(),
            wreq::header::HeaderValue::from_str(value)
                .map_err(|e| ClientError::InvalidHeader(format!("{}: {e}", kind.header())))?,
        );
    }
    Ok(())
}

impl HifToken {
    pub(crate) fn new(
        kind: HifKind,
        http: wreq::Client,
        url: String,
        headers: wreq::header::HeaderMap,
    ) -> Arc<Self> {
        Arc::new(Self {
            kind,
            http,
            url,
            headers,
            state: Mutex::new(State::default()),
        })
    }

    /// 写入缓存（仅测试用：避免单测触网）
    #[cfg(test)]
    async fn seed_for_test(&self, value: &str, ttl: Duration) {
        let mut state = self.state.lock().await;
        state.cached = Some(Cached {
            value: value.to_string(),
            expires_at: Instant::now() + ttl,
        });
        state.next_attempt_at = None;
        state.failures = 0;
    }

    /// 取可用令牌；取不到返回 `None`（调用方照常发起业务请求）
    pub(crate) async fn value(&self) -> Option<String> {
        let mut state = self.state.lock().await;
        let now = Instant::now();

        if let Some(cached) = &state.cached
            && cached.expires_at > now
        {
            return Some(cached.value.clone());
        }
        // 退避期内：沿用旧值（若有），不再发请求
        if let Some(next) = state.next_attempt_at
            && next > now
        {
            return state.cached.as_ref().map(|c| c.value.clone());
        }

        match self.fetch().await {
            Ok((value, ttl)) => {
                debug!(
                    target: "ds_core::client",
                    "hif token refreshed: {} ({}), url={}, ttl={}s",
                    self.kind.label(), self.kind.cache_key(), self.url, ttl.as_secs()
                );
                state.cached = Some(Cached {
                    value: value.clone(),
                    expires_at: now + ttl,
                });
                state.next_attempt_at = None;
                state.failures = 0;
                Some(value)
            }
            Err(e) => {
                warn!(
                    target: "ds_core::client",
                    "hif token fetch failed ({}, failures={}, backoff={}s): {}",
                    self.url,
                    state.failures,
                    failure_backoff(state.failures).as_secs(),
                    e
                );
                state.failures = state.failures.saturating_add(1);
                state.next_attempt_at = Some(now + failure_backoff(state.failures));
                state.cached.as_ref().map(|c| c.value.clone())
            }
        }
    }

    /// 主动预热（账号初始化时调用，避免首个业务请求才现取）
    pub(crate) async fn warm_up(&self) {
        // 失败细节已在 value() 内记录；这里只观测结果，业务请求照常继续
        if self.value().await.is_none() {
            debug!(
                target: "ds_core::client",
                "HIF 预热未取到令牌，首个请求将不带 {}",
                self.kind.label()
            );
        }
    }

    async fn fetch(&self) -> Result<(String, Duration), ClientError> {
        let resp = self
            .http
            .get(&self.url)
            .headers(self.headers.clone())
            .timeout(FETCH_TIMEOUT)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Status {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }

        let ttl = resp
            .headers()
            .get("x-hif-ttl")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(Duration::from_secs(DEFAULT_TTL_SECS), Duration::from_secs);

        let body = resp.text().await?;
        let value = parse_value(&body)?;
        // 提前 REFRESH_MARGIN 刷新；TTL 很短时至少保留 1s 有效期
        let ttl = ttl
            .saturating_sub(REFRESH_MARGIN)
            .max(Duration::from_secs(1));
        Ok((value, ttl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_envelope() {
        let body = r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"",
            "biz_data":{"value":"ALytJQRzYqprAAjt5gs6SzkWIc40r551S4+1cQZ/o/Pmc8t6EoPdAu4=.bf4ojFDXrmksVRYz"}}}"#;
        assert_eq!(
            parse_value(body).unwrap(),
            "ALytJQRzYqprAAjt5gs6SzkWIc40r551S4+1cQZ/o/Pmc8t6EoPdAu4=.bf4ojFDXrmksVRYz"
        );
    }

    fn test_headers() -> wreq::header::HeaderMap {
        wreq::header::HeaderMap::new()
    }

    fn test_registry() -> Arc<HifRegistry> {
        HifRegistry::new(
            wreq::Client::new(),
            "https://hif-leim.example/query".to_string(),
            "https://hif-dliq.example/query".to_string(),
            test_headers(),
        )
    }

    /// 两个端点都有值时，两个头都要下发（对齐官方客户端的头提供者）
    #[tokio::test]
    async fn both_tokens_are_attached() {
        let reg = test_registry();
        let tokens = reg.tokens_for("device-1");
        tokens
            .leim
            .seed_for_test("LEIM-VALUE", Duration::from_mins(5))
            .await;
        tokens
            .dliq
            .seed_for_test("DLIQ-VALUE", Duration::from_mins(5))
            .await;

        let mut headers = test_headers();
        insert_tokens(&mut headers, &tokens.values().await).unwrap();
        assert_eq!(headers["x-hif-leim"], "LEIM-VALUE");
        assert_eq!(headers["x-hif-dliq"], "DLIQ-VALUE");
    }

    /// 只有一个端点取到值时（本网络 dliq 为 NXDOMAIN 的情形）只下发该头，且不报错
    #[tokio::test]
    async fn missing_token_is_skipped() {
        let reg = test_registry();
        let tokens = reg.tokens_for("device-2");
        tokens
            .leim
            .seed_for_test("ONLY-LEIM", Duration::from_mins(5))
            .await;

        let values = tokens.values().await;
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, HifKind::Leim);

        let mut headers = test_headers();
        insert_tokens(&mut headers, &values).unwrap();
        assert_eq!(headers["x-hif-leim"], "ONLY-LEIM");
        assert!(headers.get("x-hif-dliq").is_none());
    }

    /// 每个设备身份一套令牌（多账号不能共用同一份风控令牌）
    #[test]
    fn tokens_are_per_device() {
        let reg = test_registry();
        assert!(Arc::ptr_eq(&reg.tokens_for("d1"), &reg.tokens_for("d1")));
        assert!(!Arc::ptr_eq(&reg.tokens_for("d1"), &reg.tokens_for("d2")));
    }

    /// 头名/缓存键与官方前端一致（改错会导致上游看到非官方头）
    #[test]
    fn header_names_match_official_client() {
        assert_eq!(HifKind::Leim.header(), "X-Hif-Leim");
        assert_eq!(HifKind::Dliq.header(), "X-Hif-Dliq");
        assert_eq!(HifKind::Leim.cache_key(), "hif_leim_cached");
        assert_eq!(HifKind::Dliq.cache_key(), "hif_dliq_cached");
    }

    /// 失败退避：指数增长且有上限（30s → 60s → … → 600s 封顶）
    #[test]
    fn failure_backoff_grows_and_caps() {
        assert_eq!(failure_backoff(1), Duration::from_secs(30));
        assert_eq!(failure_backoff(2), Duration::from_mins(1));
        assert_eq!(failure_backoff(5), Duration::from_mins(8));
        assert_eq!(failure_backoff(6), MAX_FAILURE_BACKOFF);
        assert_eq!(failure_backoff(50), MAX_FAILURE_BACKOFF);
    }

    #[test]
    fn rejects_biz_error_and_empty_value() {
        let biz_err = r#"{"code":0,"data":{"biz_code":7,"biz_msg":"rate limit","biz_data":null}}"#;
        let e = parse_value(biz_err).unwrap_err();
        assert!(matches!(e, ClientError::Business { code: 7, .. }));

        let empty = r#"{"code":0,"data":{"biz_code":0,"biz_msg":"","biz_data":{"value":""}}}"#;
        assert!(parse_value(empty).is_err());

        let missing = r#"{"code":0,"data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#;
        assert!(parse_value(missing).is_err());

        assert!(parse_value("not json").is_err());
    }
}

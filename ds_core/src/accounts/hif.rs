//! HIF 风控令牌 —— 对齐真实客户端的 `x-hif-leim`
//!
//! 真实 Web 客户端启动后会轮询 `https://hif-leim.deepseek.com/query`（**无鉴权**），
//! 把响应 `data.biz_data.value` 缓存进 localStorage（`hif_leim_cached`），
//! 有效期取响应头 `x-hif-ttl`（默认 600s），并在 **completion（SSE）请求**上以
//! `x-hif-leim` 头回传（前端源码里该头部由 `addSSEHeader` 注入）。
//!
//! 也就是说：请求 `/chat/completion` 时缺少该头，上游可以直接判定请求并非来自
//! 官方客户端 —— 这是第三方反代最容易漏掉、也最难猜到的风控信号。
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
/// 取令牌失败后的退避：避免每个请求都白等一次 3s 超时
const FAILURE_BACKOFF: Duration = Duration::from_mins(1);

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
}

/// 单个 HIF 令牌端点（leim / dliq）的取值与缓存
///
/// 令牌与「设备 + 出口 IP」绑定，因此**按设备（X-Device-Id）分别缓存**：
/// 真实客户端一个浏览器 profile 一个设备身份，代理侧同理按账号派生设备身份，
/// 多账号共用同一个令牌会把它们关联成同一台设备。
pub(crate) struct HifToken {
    http: wreq::Client,
    url: String,
    /// 客户端拟态头（与业务请求一致，但不带 Authorization）
    headers: wreq::header::HeaderMap,
    state: Mutex<State>,
}

/// 按设备身份分发 HIF 令牌（每个 X-Device-Id 一份独立缓存与刷新周期）
pub(crate) struct HifRegistry {
    http: wreq::Client,
    url: String,
    /// 除 `X-Device-Id` 之外的客户端拟态头模板
    headers: wreq::header::HeaderMap,
    tokens: DashMap<String, Arc<HifToken>>,
}

impl HifRegistry {
    pub(crate) fn new(
        http: wreq::Client,
        url: String,
        headers: wreq::header::HeaderMap,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            url,
            headers,
            tokens: DashMap::new(),
        })
    }

    /// 取某设备身份的令牌句柄（首次调用时创建，之后复用同一份缓存）
    pub(crate) fn token_for(&self, device_id: &str) -> Arc<HifToken> {
        if let Some(token) = self.tokens.get(device_id) {
            return Arc::clone(token.value());
        }
        let mut headers = self.headers.clone();
        if let Ok(value) = wreq::header::HeaderValue::from_str(device_id) {
            headers.insert("X-Device-Id", value);
        }
        let token = HifToken::new(self.http.clone(), self.url.clone(), headers);
        Arc::clone(
            self.tokens
                .entry(device_id.to_string())
                .or_insert(token)
                .value(),
        )
    }
}

impl HifToken {
    pub(crate) fn new(
        http: wreq::Client,
        url: String,
        headers: wreq::header::HeaderMap,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            url,
            headers,
            state: Mutex::new(State::default()),
        })
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
                    "hif token refreshed: url={}, ttl={}s", self.url, ttl.as_secs()
                );
                state.cached = Some(Cached {
                    value: value.clone(),
                    expires_at: now + ttl,
                });
                state.next_attempt_at = None;
                Some(value)
            }
            Err(e) => {
                warn!(
                    target: "ds_core::client",
                    "hif token fetch failed ({}): {}", self.url, e
                );
                state.next_attempt_at = Some(now + FAILURE_BACKOFF);
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
                "HIF 预热未取到令牌，首个请求将不带 x-hif-leim"
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

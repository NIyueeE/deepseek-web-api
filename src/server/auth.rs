//! 鉴权模块 —— JWT 签发/验证 + 登录失败率限制

use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::store::StoreManager;

type HmacSha256 = Hmac<Sha256>;

// ── JWT ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    pub iat: u64,
    pub exp: u64,
}

const JWT_HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;
const JWT_EXPIRY_SECS: u64 = 24 * 3600;

fn base64url_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn base64url_decode(data: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .ok()
}

/// 签发 JWT
pub async fn sign_jwt(store: &StoreManager) -> Option<String> {
    let secret = store.jwt_secret().await?;
    let now = epoch_secs();

    let payload = serde_json::to_vec(&TokenClaims {
        sub: "admin".to_string(),
        iat: now,
        exp: now + JWT_EXPIRY_SECS,
    })
    .ok()?;

    let header_b64 = base64url_encode(JWT_HEADER.as_bytes());
    let payload_b64 = base64url_encode(&payload);
    let signing_input = format!("{header_b64}.{payload_b64}");

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(signing_input.as_bytes());
    let sig_b64 = base64url_encode(&mac.finalize().into_bytes());

    let token = format!("{signing_input}.{sig_b64}");

    // 更新 jwt_issued_at（用于吊销旧 token）
    store.set_jwt_issued_at(now).await;
    Some(token)
}

/// 验证 JWT，返回是否有效
pub async fn verify_jwt(store: &StoreManager, token: &str) -> bool {
    let Some(secret) = store.jwt_secret().await else {
        return false;
    };

    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return false;
    }

    // 验证 HMAC-SHA256 签名
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(signing_input.as_bytes());
    let expected = mac.finalize().into_bytes();

    let Some(sig_bytes) = base64url_decode(parts[2]) else {
        return false;
    };

    // CtOutput deref 到 [u8]，可以直接比较
    if &*expected != sig_bytes.as_slice() {
        return false;
    }

    // 解析 payload
    let Some(payload_bytes) = base64url_decode(parts[1]) else {
        return false;
    };

    // 复用签发侧的 TokenClaims：字段完全一致，避免再定义一份 payload 结构
    let payload: TokenClaims = match serde_json::from_slice(&payload_bytes) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // 过期检查（60 秒 leeway，对齐原 jsonwebtoken 行为）
    let now = epoch_secs();
    if now > payload.exp + 60 {
        return false;
    }

    // 吊销检查：token 的 iat 必须 >= 存储的 jwt_issued_at
    // 改密码时会更新 jwt_issued_at，使旧 token 失效
    if let Some(min_iat) = store.jwt_issued_at().await
        && payload.iat < min_iat
    {
        return false;
    }

    true
}

// ── 登录失败率限制 ────────────────────────────────────────────────────────

/// 最大失败次数
const MAX_FAILURES: u64 = 5;
/// 锁定时长
const LOCKOUT_SECS: u64 = 300; // 5 分钟

pub struct LoginLimiter {
    fail_count: AtomicU64,
    locked_until: AtomicU64, // epoch secs，0 表示未锁定
}

impl LoginLimiter {
    pub const fn new() -> Self {
        Self {
            fail_count: AtomicU64::new(0),
            locked_until: AtomicU64::new(0),
        }
    }

    /// 检查是否被锁定
    pub fn is_locked(&self) -> bool {
        let until = self.locked_until.load(Ordering::Relaxed);
        if until == 0 {
            return false;
        }
        if epoch_secs() >= until {
            // 锁定已过期，重置
            self.locked_until.store(0, Ordering::Relaxed);
            self.fail_count.store(0, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// 记录一次失败
    pub fn record_failure(&self) {
        let count = self.fail_count.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= MAX_FAILURES {
            self.locked_until
                .store(epoch_secs() + LOCKOUT_SECS, Ordering::Relaxed);
        }
    }

    /// 记录成功，重置计数
    pub fn record_success(&self) {
        self.fail_count.store(0, Ordering::Relaxed);
        self.locked_until.store(0, Ordering::Relaxed);
    }

    /// 剩余锁定秒数
    pub fn remaining_lock_secs(&self) -> u64 {
        let until = self.locked_until.load(Ordering::Relaxed);
        if until == 0 {
            return 0;
        }
        let now = epoch_secs();
        until.saturating_sub(now)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── 高层管理函数 ──────────────────────────────────────────────────────────

/// 首次设置管理员密码，返回 JWT token
pub async fn setup_admin(
    store: &StoreManager,
    limiter: &LoginLimiter,
    password: &str,
) -> Result<String, String> {
    if store.has_password().await {
        return Err("密码已设置，请使用登录接口".into());
    }

    if limiter.is_locked() {
        return Err(format!(
            "请求次数过多，请 {} 秒后重试",
            limiter.remaining_lock_secs()
        ));
    }

    if password.len() < 6 {
        limiter.record_failure();
        return Err("密码长度至少 6 位".into());
    }

    let password_hash = super::store::hash_password(password);
    let jwt_secret = super::store::generate_hex_secret();
    store
        .save_admin(password_hash, jwt_secret, 0)
        .await
        .map_err(|e| format!("保存失败: {e}"))?;

    sign_jwt(store).await.ok_or_else(|| "JWT 签发失败".into())
}

/// 密码登录，返回 JWT token
pub async fn login_admin(
    store: &StoreManager,
    limiter: &LoginLimiter,
    password: &str,
) -> Result<String, String> {
    if !store.has_password().await {
        return Err("未设置密码，请先使用 setup 接口".into());
    }

    if limiter.is_locked() {
        return Err(format!(
            "登录失败次数过多，请 {} 秒后重试",
            limiter.remaining_lock_secs()
        ));
    }

    if store.verify_password(password).await {
        limiter.record_success();
        sign_jwt(store).await.ok_or_else(|| "JWT 签发失败".into())
    } else {
        limiter.record_failure();
        Err("密码错误".into())
    }
}

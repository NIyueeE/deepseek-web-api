//! 持久化存储 —— Config-based admin/auth 数据 + stats.json 的原子读写

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{info, warn};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::Config;

/// 管理 stats.json 的数据
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StatsStore {
    pub total_requests: u64,
    pub success_requests: u64,
    pub failed_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// 按模型拆分的统计（重启可恢复）
    #[serde(default)]
    pub model_stats: std::collections::HashMap<String, ModelStatsData>,
    /// 按 API Key 拆分的统计（重启可恢复，key 为脱敏后的前缀）
    #[serde(default)]
    pub key_stats: std::collections::HashMap<String, KeyStatsData>,
    /// 最近 N 条请求日志（重启可恢复）
    #[serde(default)]
    pub request_logs: Vec<RequestLogData>,
}

/// 持久化的模型统计数据
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ModelStatsData {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: u64,
}

/// 持久化的 API Key 统计数据
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeyStatsData {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: u64,
}

/// 持久化的请求日志条目
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RequestLogData {
    pub timestamp: u64,
    pub request_id: String,
    pub model: String,
    pub api_key: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    pub success: bool,
}

/// 运行时存储管理器（admin + api_keys → Config，stats → stats.json）
pub struct StoreManager {
    config_path: PathBuf,
    config: Arc<RwLock<Config>>,
    base_dir: PathBuf,
    pub stats: Arc<RwLock<StatsStore>>,
}

impl StoreManager {
    pub fn new(base_dir: &Path, config_path: &Path, config: Arc<RwLock<Config>>) -> Self {
        let stats_path = base_dir.join("stats.json");
        let stats = if stats_path.exists() {
            match fs::read_to_string(&stats_path) {
                Ok(content) if !content.trim().is_empty() => {
                    match serde_json::from_str::<StatsStore>(&content) {
                        Ok(s) => {
                            info!(target: "store", "loaded stats.json");
                            s
                        }
                        Err(e) => {
                            warn!(target: "store", "failed to parse stats.json: {e}, using zeros");
                            StatsStore::default()
                        }
                    }
                }
                Ok(_) => {
                    info!(target: "store", "stats.json is empty, using zeros");
                    StatsStore::default()
                }
                Err(e) => {
                    warn!(target: "store", "failed to read stats.json: {e}, using zeros");
                    StatsStore::default()
                }
            }
        } else {
            info!(target: "store", "stats.json not found, using zeros");
            StatsStore::default()
        };

        Self {
            config_path: config_path.to_path_buf(),
            config,
            base_dir: base_dir.to_path_buf(),
            stats: Arc::new(RwLock::new(stats)),
        }
    }

    /// 检查是否已设置密码
    pub async fn has_password(&self) -> bool {
        !self.config.read().await.admin.password_hash.is_empty()
    }

    /// 验证密码
    pub async fn verify_password(&self, plain: &str) -> bool {
        let guard = self.config.read().await;
        bcrypt::verify(plain, &guard.admin.password_hash).unwrap_or(false)
    }

    /// 获取 JWT 密钥
    pub async fn jwt_secret(&self) -> Option<String> {
        let guard = self.config.read().await;
        if guard.admin.jwt_secret.is_empty() {
            None
        } else {
            Some(guard.admin.jwt_secret.clone())
        }
    }

    /// 获取最近一次 JWT 签发时间（用于吊销旧 token）
    pub async fn jwt_issued_at(&self) -> Option<u64> {
        let iat = self.config.read().await.admin.jwt_issued_at;
        (iat > 0).then_some(iat)
    }

    /// 更新 jwt_issued_at 并持久化
    pub async fn set_jwt_issued_at(&self, iat: u64) {
        let mut guard = self.config.write().await;
        guard.admin.jwt_issued_at = iat;
        if let Err(e) = guard.save(&self.config_path) {
            log::warn!(target: "http::server", "持久化 jwt_issued_at 失败: {e}");
        }
        drop(guard);
    }

    /// 保存 admin 配置（密码哈希、JWT 密钥等）
    pub async fn save_admin(
        &self,
        password_hash: String,
        jwt_secret: String,
        jwt_issued_at: u64,
    ) -> anyhow::Result<()> {
        let mut guard = self.config.write().await;
        guard.admin.password_hash = password_hash;
        guard.admin.jwt_secret = jwt_secret;
        guard.admin.jwt_issued_at = jwt_issued_at;
        let result = guard.save(&self.config_path);
        drop(guard);
        Ok(result?)
    }

    /// 查找 API Key 是否有效
    pub async fn is_valid_api_key(&self, key: &str) -> bool {
        let guard = self.config.read().await;
        guard.api_keys.iter().any(|k| k.key == key)
    }

    /// 加载持久化的统计数据
    pub async fn load_stats(&self) -> StatsStore {
        self.stats.read().await.clone()
    }

    /// 保存 stats.json
    pub async fn save_stats(&self, store: &StatsStore) -> anyhow::Result<()> {
        let path = self.base_dir.join("stats.json");
        write_json_file(&path, store)?;
        *self.stats.write().await = store.clone();
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// 原子写入 JSON 文件：先写 .tmp 再 rename
fn write_json_file<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<()> {
    let tmp_path = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(data)?;
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, path)?;
    // 设置文件权限 0600（仅 owner 可读写）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// 生成随机 hex 字符串（32 字节 = 64 hex 字符）
pub fn generate_hex_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    hex::encode(&bytes)
}

/// 对密码进行 bcrypt 哈希
pub fn hash_password(plain: &str) -> String {
    bcrypt::hash(plain, 12).expect("bcrypt hash 不应失败")
}

// hex 编码辅助（避免额外依赖）
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(out, "{b:02x}");
        }
        out
    }
}

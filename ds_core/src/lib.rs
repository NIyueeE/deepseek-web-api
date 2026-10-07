//! DeepSeek 核心模块 —— OpenAI API 到 DeepSeek 的适配层
//!
//! 对外暴露最小接口：DsCore, CoreError, ChatRequest, DsCoreConfig, AccountConfig

mod accounts;
mod chat;
mod config;

pub use accounts::PoolError;
pub use accounts::pool::AccountStatus;
pub use accounts::{
    ChatSessionInfo, ClientIdentity, DsClient, FetchSessionsData, HifConfig, LoginPayload,
    random_device_uuid,
};
pub use chat::{ChatRequest, ChatResponse, FilePayload, StreamEvent};
pub use config::{AccountConfig, DsCoreConfig};

use accounts::Accounts;
use chat::Chat;
use std::sync::Arc;

/// 内核层错误类型
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 服务过载：所有账号都在忙或不健康
    #[error("no available account")]
    Overloaded,

    /// PoW 计算失败
    #[error("proof of work failed: {0}")]
    ProofOfWorkFailed(#[from] accounts::PowError),

    /// 提供商错误：网络、业务错误、Token 失效等
    #[error("provider: {0}")]
    ProviderError(String),

    /// 流处理错误：连接中断等
    #[error("stream error: {0}")]
    Stream(String),
}

impl From<accounts::ClientError> for CoreError {
    fn from(e: accounts::ClientError) -> Self {
        CoreError::ProviderError(e.to_string())
    }
}

pub struct DsCore {
    accounts: Arc<Accounts>,
    chat: Chat,
}

impl DsCore {
    pub async fn new(
        config: &DsCoreConfig,
        account_creds: Vec<AccountConfig>,
    ) -> Result<Self, CoreError> {
        let accounts = Accounts::new(config, account_creds).await?;
        let chat = Chat::new(Arc::clone(&accounts), config);

        Ok(Self { accounts, chat })
    }

    /// 发起对话请求，返回 SSE 字节流 + 账号标识
    ///
    /// 流结束或丢弃时自动释放账号
    pub async fn v0_chat(
        &self,
        req: ChatRequest,
        request_id: &str,
    ) -> Result<ChatResponse, CoreError> {
        self.chat.v0_chat(req, request_id).await
    }

    #[must_use]
    pub fn account_statuses(&self) -> Vec<AccountStatus> {
        self.accounts.account_statuses()
    }

    /// 获取详细账号状态（含滑动窗口统计、请求间隔等）
    #[must_use]
    pub fn account_statuses_detailed(&self) -> Vec<AccountStatus> {
        self.accounts.account_statuses_detailed()
    }

    /// 拉取账号的会话列表（分页；`updated_at` 为游标，None = 第一页）
    pub async fn fetch_sessions(
        &self,
        updated_at: Option<f64>,
    ) -> Result<accounts::FetchSessionsData, CoreError> {
        self.accounts.fetch_sessions(updated_at).await
    }

    /// 动态添加账号
    pub async fn add_account(&self, creds: &AccountConfig) -> Result<String, PoolError> {
        self.accounts.add_account(creds).await
    }

    /// 动态移除账号
    pub async fn remove_account(&self, email_or_mobile: &str) -> Result<String, PoolError> {
        self.accounts.remove_account(email_or_mobile).await
    }

    /// 标记账号为 Error 状态
    pub fn mark_error(&self, email_or_mobile: &str) {
        self.accounts.mark_error(email_or_mobile);
    }

    /// 手动重新登录指定账号
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        self.accounts.re_login_single(email_or_mobile).await
    }

    /// 优雅关闭：清理所有账号的 session
    pub async fn shutdown(&self) {
        self.chat.shutdown().await;
        self.accounts.shutdown().await;
    }

    pub async fn reload_config(&self, config: &DsCoreConfig) -> Result<(), CoreError> {
        self.accounts.reload_config(config).await
    }
}

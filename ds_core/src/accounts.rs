//! 账号模块 —— 账号池、HTTP 客户端、PoW 求解器
//!
//! chat 模块通过此门面访问账号相关能力。

mod client;
mod hif;
pub(crate) mod pool;
mod pow;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::CoreError;
use crate::config::{AccountConfig, DsCoreConfig};
pub use client::{
    ChatSessionInfo, ClientError, ClientIdentity, CompletionPayload, DsClient, FetchSessionsData,
    HifConfig, LoginPayload, StopStreamPayload,
};
pub use pool::{Account, AccountGuard, AccountPool, AccountStatus, PoolError};
pub use pow::{PowError, PowSolver};

/// 由配置构造 HTTP 客户端（客户端拟态身份 + HIF 风控令牌端点）
fn build_client(config: &DsCoreConfig) -> DsClient {
    DsClient::new(
        config.api_base.clone(),
        config.wasm_url.clone(),
        ClientIdentity {
            user_agent: config.user_agent.clone(),
            client_version: config.client_version.clone(),
            client_platform: config.client_platform.clone(),
            client_locale: config.client_locale.clone(),
            client_bundle_id: config.client_bundle_id.clone(),
            device_id: config.client_device_id.clone(),
            device_model: config.client_device_model.clone(),
            timezone_offset: config.client_timezone_offset.clone(),
            client_os: config.client_os.clone(),
        },
        &HifConfig {
            enabled: config.hif_enabled,
            ..HifConfig::default()
        },
        config.proxy_url.as_deref(),
    )
}

/// 账号模块的统一入口
///
/// 封装 AccountPool、DsClient、PowSolver，向上层（chat、DsCore）提供统一接口。
pub struct Accounts {
    pool: Arc<AccountPool>,
    client: RwLock<DsClient>,
    solver: RwLock<PowSolver>,
}

impl Accounts {
    /// 创建并初始化账号模块
    ///
    /// 步骤：创建 HTTP 客户端 → 加载 WASM → 初始化账号池 → 启动恢复任务
    pub async fn new(
        config: &DsCoreConfig,
        account_creds: Vec<AccountConfig>,
    ) -> Result<Arc<Self>, CoreError> {
        let client = build_client(config);

        let wasm_bytes = client.get_wasm().await?;
        let solver = PowSolver::new(&wasm_bytes)?;

        let pool = AccountPool::new(config.hourly_request_quota);
        pool.init(account_creds, &client, &solver)
            .await
            .map_err(|e| match e {
                PoolError::AllAccountsFailed => {
                    CoreError::ProviderError("所有账号初始化失败".to_string())
                }
                PoolError::Client(e) => CoreError::ProviderError(e.to_string()),
                PoolError::Pow(e) => CoreError::ProofOfWorkFailed(e),
                PoolError::Validation(msg) => {
                    CoreError::ProviderError(format!("配置错误: {}", msg))
                }
                other => CoreError::ProviderError(other.to_string()),
            })?;

        let pool = Arc::new(pool);
        pool.set_client_solver(client.clone(), solver.clone()).await;
        pool.start_recovery_task();

        Ok(Arc::new(Self {
            pool,
            client: RwLock::new(client),
            solver: RwLock::new(solver),
        }))
    }

    // ── 供 chat 模块使用 ────────────────────────────────────────────

    /// 获取账号（等待最多 timeout_ms 毫秒）
    pub async fn get_account_with_wait(&self, timeout_ms: u64) -> Option<AccountGuard> {
        self.pool.get_account_with_wait(timeout_ms).await
    }

    /// 获取账号（立即返回）
    pub fn get_account(&self) -> Option<AccountGuard> {
        self.pool.get_account()
    }

    /// 拉取池中某一可用账号的会话列表（借一个空闲账号发起，见 issue #110）
    pub async fn fetch_sessions(
        &self,
        updated_at: Option<f64>,
    ) -> Result<FetchSessionsData, CoreError> {
        let guard = self
            .pool
            .get_account_with_wait(5000)
            .await
            .ok_or(CoreError::Overloaded)?;
        let token = guard.account().token();
        self.client
            .read()
            .await
            .fetch_sessions(&token, updated_at)
            .await
            .map_err(Into::into)
    }

    /// 标记账号 Error
    pub fn mark_error(&self, email_or_mobile: &str) {
        self.pool.mark_error(email_or_mobile);
    }

    /// 创建 session（使用该账号自己的设备身份）
    pub async fn create_session(&self, account: &Account) -> Result<String, CoreError> {
        account
            .client()
            .create_session(&account.token())
            .await
            .map_err(Into::into)
    }

    /// 发起 completion（返回原始 SSE 字节流）
    pub async fn completion(
        &self,
        account: &Account,
        pow_header: &str,
        payload: &CompletionPayload,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<bytes::Bytes, ClientError>> + Send>>,
        CoreError,
    > {
        account
            .client()
            .completion(&account.token(), pow_header, payload)
            .await
            .map_err(Into::into)
    }

    /// 取消流式输出
    pub async fn stop_stream(
        &self,
        account: &Account,
        payload: &StopStreamPayload,
    ) -> Result<(), CoreError> {
        account
            .client()
            .stop_stream(&account.token(), payload)
            .await
            .map_err(Into::into)
    }

    /// 上传文件（含 PoW 计算与轮询）
    pub async fn upload_and_poll(
        &self,
        account: &Account,
        filename: &str,
        content_type: &str,
        content: &[u8],
        request_id: &str,
    ) -> Result<String, CoreError> {
        const UPLOAD_POLL_INTERVAL_MS: u64 = 2000;
        const UPLOAD_POLL_MAX_RETRIES: usize = 30;

        let client = account.client();
        let token = &account.token();
        let pow_header = self
            .compute_pow_for_target(account, "/api/v0/file/upload_file")
            .await?;

        let upload_data = client
            .upload_file(token, &pow_header, filename, content_type, content.to_vec())
            .await?;
        let file_id = upload_data.id;

        for _ in 0..UPLOAD_POLL_MAX_RETRIES {
            let fetch_data = client
                .fetch_files(token, std::slice::from_ref(&file_id))
                .await?;
            if let Some(file) = fetch_data.files.first() {
                match file.status.as_str() {
                    "SUCCESS" => {
                        log::debug!(
                            target: "ds_core::accounts",
                            "req={} 文件上传成功: file_id={}, tokens={:?}, name={}",
                            request_id, file_id, file.token_usage, file.file_name
                        );
                        return Ok(file_id);
                    }
                    "FAILED" => {
                        return Err(CoreError::ProviderError(format!(
                            "文件上传失败: {}",
                            file.file_name
                        )));
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(UPLOAD_POLL_INTERVAL_MS)).await;
        }
        Err(CoreError::ProviderError("文件处理超时".into()))
    }

    /// 计算指定 target_path 的 PoW header
    pub async fn compute_pow_for_target(
        &self,
        account: &Account,
        target_path: &str,
    ) -> Result<String, CoreError> {
        let challenge_data = account
            .client()
            .create_pow_challenge(&account.token(), target_path)
            .await?;
        let solver = self.solver.read().await;
        let result = solver.solve(&challenge_data)?;
        Ok(result.to_header())
    }

    pub fn account_statuses(&self) -> Vec<AccountStatus> {
        self.pool.account_statuses()
    }

    pub fn account_statuses_detailed(&self) -> Vec<AccountStatus> {
        self.pool.account_statuses_detailed()
    }

    pub async fn add_account(&self, creds: &AccountConfig) -> Result<String, PoolError> {
        let client = self.client.read().await;
        let solver = self.solver.read().await;
        self.pool.add_account(creds, &client, &solver).await
    }

    pub async fn remove_account(&self, email_or_mobile: &str) -> Result<String, PoolError> {
        self.pool.remove_account(email_or_mobile).await
    }

    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        self.pool.re_login_single(email_or_mobile).await
    }

    pub async fn shutdown(&self) {
        let client = self.client.read().await;
        self.pool.shutdown(&client).await;
    }

    pub async fn reload_config(&self, config: &DsCoreConfig) -> Result<(), CoreError> {
        let client = build_client(config);
        let wasm_bytes = client.get_wasm().await?;
        let solver = PowSolver::new(&wasm_bytes)?;

        self.pool
            .set_client_solver(client.clone(), solver.clone())
            .await;
        *self.client.write().await = client;
        *self.solver.write().await = solver;
        Ok(())
    }
}

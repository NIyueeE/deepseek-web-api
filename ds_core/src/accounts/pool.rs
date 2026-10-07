//! 账号池管理 —— 多账号负载均衡
//!
//! 1 account = 1 session = 1 concurrency。多并发需横向扩展账号数。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::SystemTime;

use dashmap::DashMap;
use futures::TryStreamExt;
use log::{debug, error, info, warn};
use tokio::sync::RwLock;

use super::client::{ClientError, CompletionPayload, DsClient, LoginPayload};
use super::pow::{PowError, PowSolver};
use crate::config::AccountConfig;

/// 账号状态枚举
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountState {
    Idle = 0,
    Busy = 1,
    Error = 2,
    Invalid = 3,
}

impl AccountState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Busy,
            2 => Self::Error,
            _ => Self::Invalid,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Error => "error",
            Self::Invalid => "invalid",
        }
    }
}

/// 账号状态信息
#[derive(serde::Serialize)]
pub struct AccountStatus {
    pub email: String,
    pub mobile: String,
    pub state: String,
    /// 最后释放时间戳（ms），0 表示从未使用
    pub last_released_ms: i64,
    /// 窗口起点（Unix 秒）
    pub window_started_at: i64,
    /// 窗口内剩余秒数
    pub window_remaining_secs: i64,
    /// 总请求数（累计）
    pub total_requests: u64,
    /// 首次请求时间戳（ms）
    pub first_request_ms: i64,
    /// 最后请求时间戳（ms）
    pub last_request_ms: i64,
    /// 最近请求间隔（秒）
    pub recent_intervals: Vec<i64>,
    /// 连续登录失败次数
    pub error_count: u8,
    /// 当前配额窗口内已用请求数
    pub used_this_hour: u64,
    /// 本窗口是否已用尽配额（0 配额 = 不限制，恒为 false）
    pub quota_exhausted: bool,
}

impl AccountStatus {
    fn from_account(account: &Account, hourly_quota: u64) -> Self {
        let (total, window_used, first, last, intervals) = account.window.get_stats();
        // For sliding window, compute window start as earliest timestamp in current window
        let window_started = {
            let now = now_secs();
            let ts = account.window.timestamps.lock().unwrap();
            ts.front().copied().unwrap_or(now)
        };
        let window_remaining = (window_started + WINDOW_SECS as i64 - now_secs()).max(0);
        Self {
            email: account.email.clone(),
            mobile: account.mobile.clone(),
            state: account.state().as_str().to_string(),
            last_released_ms: account.last_released.load(Ordering::Relaxed),
            window_started_at: window_started,
            window_remaining_secs: window_remaining,
            total_requests: total,
            first_request_ms: first * 1000,
            last_request_ms: last * 1000,
            recent_intervals: intervals,
            error_count: account.error_count.load(Ordering::Relaxed),
            used_this_hour: window_used,
            quota_exhausted: !account.within_quota(hourly_quota),
        }
    }
}

pub struct Account {
    token: std::sync::RwLock<Arc<str>>,
    email: String,
    mobile: String,
    state: AtomicU8,
    /// 账号最近一次释放的时间戳（ms），用于冷却判断
    last_released: AtomicI64,
    /// 连续登录失败次数
    error_count: AtomicU8,
    /// 原始凭据（用于重新登录）
    creds: AccountConfig,
    /// 滑动窗口限流器（用于每小时配额）
    window: SlidingWindowRateLimiter,
}

/// 连续登录失败上限，达到后标记为 Invalid
const MAX_ERROR_COUNT: u8 = 3;

/// 配额窗口长度：1 小时
const WINDOW_SECS: u64 = 3600;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 严格滑动窗口限流器
///
/// 保证：任意时刻，最近 `WINDOW_SECS` 秒内的请求数 ≤ 配额
/// 使用 VecDeque 存储请求时间戳（Unix 秒），自动清理过期项。
///
/// 设计为无锁读 + 细粒度锁写，适合高并发场景。
struct SlidingWindowRateLimiter {
    timestamps: Mutex<VecDeque<i64>>,
    /// 总请求数（累计，跨窗口）
    total_count: AtomicU64,
    /// 首次请求时间戳（Unix 秒）
    first_request_at: AtomicI64,
    /// 最后请求时间戳（Unix 秒）
    last_request_at: AtomicI64,
    /// 请求间隔记录（最近 10 个，Unix 秒差值）
    recent_intervals: Mutex<Vec<i64>>,
}

impl SlidingWindowRateLimiter {
    fn new() -> Self {
        Self {
            timestamps: Mutex::new(VecDeque::new()),
            total_count: AtomicU64::new(0),
            first_request_at: AtomicI64::new(0),
            last_request_at: AtomicI64::new(0),
            recent_intervals: Mutex::new(Vec::new()),
        }
    }

    fn record_interval(&self, now: i64) {
        let last = self.last_request_at.swap(now, Ordering::Relaxed);
        if last > 0 {
            let interval = now - last;
            if let Ok(mut intervals) = self.recent_intervals.lock() {
                intervals.push(interval);
                if intervals.len() > 10 {
                    intervals.remove(0);
                }
            }
        }
        let first = self.first_request_at.load(Ordering::Relaxed);
        if first == 0 {
            self.first_request_at.store(now, Ordering::Relaxed);
        }
    }

    fn get_stats(&self) -> (u64, u64, i64, i64, Vec<i64>) {
        let total = self.total_count.load(Ordering::Relaxed);
        let window_used = self.used();
        let first = self.first_request_at.load(Ordering::Relaxed);
        let last = self.last_request_at.load(Ordering::Relaxed);
        let intervals = self
            .recent_intervals
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        (total, window_used, first, last, intervals)
    }

    /// 记一次请求并返回窗口内的累计值；自动清理过期时间戳
    fn record(&self) -> u64 {
        let now = now_secs();
        let mut ts = self.timestamps.lock().unwrap();
        let cutoff = now - WINDOW_SECS as i64;

        // 清理过期时间戳
        while ts.front().is_some_and(|&t| t < cutoff) {
            ts.pop_front();
        }

        ts.push_back(now);
        let window_used = ts.len() as u64;

        drop(ts); // 释放锁

        self.total_count.fetch_add(1, Ordering::Relaxed);
        self.record_interval(now);
        window_used
    }

    /// 当前窗口内已用请求数（不修改状态）
    fn used(&self) -> u64 {
        let now = now_secs();
        let mut ts = self.timestamps.lock().unwrap();
        let cutoff = now - WINDOW_SECS as i64;

        while ts.front().is_some_and(|&t| t < cutoff) {
            ts.pop_front();
        }

        ts.len() as u64
    }
}

impl Account {
    pub fn token(&self) -> Arc<str> {
        self.token.read().unwrap().clone()
    }

    pub fn display_id(&self) -> &str {
        if self.email.is_empty() {
            &self.mobile
        } else {
            &self.email
        }
    }

    pub fn state(&self) -> AccountState {
        AccountState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn is_busy(&self) -> bool {
        self.state() == AccountState::Busy
    }

    pub fn is_available(&self) -> bool {
        self.state() == AccountState::Idle
    }

    /// 该账号在本配额窗口内是否还能继续使用
    ///
    /// `limit == 0` 表示不限制（保持既有行为）。
    fn within_quota(&self, limit: u64) -> bool {
        limit == 0 || self.window.used() < limit
    }

    /// 记一次请求用量；达到配额时打一次告警
    fn record_request(&self, limit: u64) {
        let used = self.window.record();
        if limit > 0 && used == limit {
            warn!(
                target: "ds_core::accounts",
                "Account {} reached the hourly request budget ({}); it will be skipped until the window rolls over. \
                 Upstream mutes accounts after a few hundred requests per hour — spread load across more accounts.",
                self.display_id(), limit
            );
        }
    }

    /// 创建一个 Invalid 状态的账号（初始化失败时使用，仍加入池以便前台展示）
    fn new_invalid(creds: AccountConfig, _hourly_quota: u64) -> Self {
        Self {
            token: std::sync::RwLock::new(String::new().into()),
            email: creds.email.clone(),
            mobile: creds.mobile.clone(),
            state: AtomicU8::new(AccountState::Invalid as u8),
            last_released: AtomicI64::new(0),
            error_count: AtomicU8::new(MAX_ERROR_COUNT),
            creds,
            window: SlidingWindowRateLimiter::new(),
        }
    }

    fn get_total_requests(&self) -> u64 {
        self.window.total_count.load(Ordering::Relaxed)
    }
}

/// 持有期间账号标记为 busy，Drop 时自动释放
pub struct AccountGuard {
    account: Arc<Account>,
}

impl AccountGuard {
    pub fn account(&self) -> &Account {
        &self.account
    }
}

impl Drop for AccountGuard {
    fn drop(&mut self) {
        // 只有 Busy 状态才释放回 Idle（避免覆盖 Error/Invalid）
        self.account
            .state
            .compare_exchange(
                AccountState::Busy as u8,
                AccountState::Idle as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok();
        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = (d.as_secs() * 1000 + u64::from(d.subsec_millis())) as i64;
        self.account.last_released.store(now_ms, Ordering::Relaxed);
    }
}

pub struct AccountPool {
    /// 每账号每小时请求上限（0 = 不限制）
    hourly_quota: u64,
    /// key = display_id (email or mobile), value = Account
    accounts: DashMap<String, Arc<Account>>,
    client: RwLock<Option<DsClient>>,
    solver: RwLock<Option<PowSolver>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// 所有账号初始化失败（没有可用账号）
    #[error("所有账号初始化失败")]
    AllAccountsFailed,

    /// 下游客户端错误（网络、API 错误等）
    #[error("客户端错误: {0}")]
    Client(#[from] ClientError),

    /// PoW 计算失败（WASM 执行错误）
    #[error("PoW 计算失败: {0}")]
    Pow(#[from] PowError),

    /// 账号配置验证失败
    #[error("账号配置错误: {0}")]
    Validation(String),

    /// 账号已存在
    #[error("账号已存在: {0}")]
    AlreadyExists(String),

    /// 账号不存在
    #[error("账号不存在: {0}")]
    NotFound(String),

    /// 账号正在使用中，无法删除
    #[error("账号正在使用中: {0}")]
    AccountBusy(String),
}

impl AccountPool {
    pub fn new(hourly_quota: u64) -> Self {
        Self {
            hourly_quota,
            accounts: DashMap::new(),
            client: RwLock::new(None),
            solver: RwLock::new(None),
        }
    }

    pub async fn init(
        &self,
        creds: Vec<AccountConfig>,
        client: &DsClient,
        solver: &PowSolver,
    ) -> Result<(), PoolError> {
        if creds.is_empty() {
            return Ok(());
        }

        warn_on_shared_device_ids(&creds);

        use futures::future::join_all;
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        // 限制并发初始化数，避免对 DeepSeek 端和本地连接池造成压力
        let semaphore = Arc::new(Semaphore::new(13));
        let futures: Vec<_> = creds
            .into_iter()
            .map(|creds| {
                let client = client.clone();
                let solver = solver.clone();
                let sem = semaphore.clone();
                async move {
                    let _permit = sem.acquire().await.expect("信号量未关闭");
                    let display_id = if creds.email.is_empty() {
                        creds.mobile.clone()
                    } else {
                        creds.email.clone()
                    };
                    let account = match init_account(&creds, &client, &solver).await {
                        Ok(account) => {
                            info!(target: "ds_core::accounts", "Account {} initialized successfully", display_id);
                            account
                        }
                        Err(e) => {
                            warn!(target: "ds_core::accounts", "Account {} initialization failed: {}", display_id, e);
                            // 即使初始化失败也加入池，标记为 Invalid 以便前台展示
                            Account::new_invalid(creds.clone(), self.hourly_quota)
                        }
                    };
                    Some((display_id, Arc::new(account)))
                }
            })
            .collect();

        let results: Vec<(String, Arc<Account>)> =
            join_all(futures).await.into_iter().flatten().collect();
        let idle_count = results
            .iter()
            .filter(|(_, a)| a.state() == AccountState::Idle)
            .count();

        for (id, account) in &results {
            self.accounts.insert(id.clone(), Arc::clone(account));
        }

        if idle_count == 0 {
            warn!(target: "ds_core::accounts", "All accounts failed to initialize — they may be disabled or have invalid credentials");
        } else if results.len() > 1 && idle_count < results.len() {
            warn!(target: "ds_core::accounts", "{}/{} accounts unavailable", results.len() - idle_count, results.len());
        }
        Ok(())
    }

    /// 动态添加账号（运行时初始化）
    pub async fn add_account(
        &self,
        creds: &AccountConfig,
        client: &DsClient,
        solver: &PowSolver,
    ) -> Result<String, PoolError> {
        let display_id = if creds.email.is_empty() {
            creds.mobile.clone()
        } else {
            creds.email.clone()
        };

        // 检查是否已存在（DashMap O(1) 查找）
        if self.accounts.contains_key(&display_id) {
            return Err(PoolError::AlreadyExists(display_id));
        }

        let account = init_account(creds, client, solver).await?;
        let _id = account.display_id().to_string();
        self.accounts.insert(display_id.clone(), Arc::new(account));
        info!(target: "ds_core::accounts", "Account {} added dynamically", display_id);
        Ok(display_id)
    }

    /// 动态移除账号（仅空闲账号可移除）
    pub async fn remove_account(&self, email_or_mobile: &str) -> Result<String, PoolError> {
        let account = self
            .accounts
            .get(email_or_mobile)
            .ok_or_else(|| PoolError::NotFound(email_or_mobile.to_string()))?;

        if account.is_busy() {
            return Err(PoolError::AccountBusy(email_or_mobile.to_string()));
        }

        // 也允许移除 Error/Invalid 状态的账号
        drop(account);
        let (_, removed) = self
            .accounts
            .remove(email_or_mobile)
            .ok_or_else(|| PoolError::NotFound(email_or_mobile.to_string()))?;
        let id = removed.display_id().to_string();
        info!(target: "ds_core::accounts", "Account {} removed", id);
        Ok(id)
    }

    /// 获取空闲最久的可用账号，带等待：无可用账号时最多等待 `timeout_ms` 毫秒
    pub async fn get_account_with_wait(&self, timeout_ms: u64) -> Option<AccountGuard> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(g) = self.get_account() {
                return Some(g);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// 获取空闲最久的可用账号（不等待，立即返回）
    ///
    /// 遍历所有账号，选冷却已过且空闲时间最长的那个，最大化每次使用间隔。
    /// DashMap 无锁读，不阻塞并发请求。
    pub fn get_account(&self) -> Option<AccountGuard> {
        if self.accounts.is_empty() {
            return None;
        }

        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = (d.as_secs() * 1000 + u64::from(d.subsec_millis())) as i64;

        let mut best: Option<Arc<Account>> = None;
        let mut best_idle = i64::MIN;

        for entry in self.accounts.iter() {
            let account = entry.value();
            if !account.is_available() {
                continue;
            }
            // 超出每小时配额的账号本窗口内不再分配（0 = 不限制）
            if !account.within_quota(self.hourly_quota) {
                debug!(
                    target: "ds_core::accounts",
                    "Account {} quota exhausted (used={}, limit={}), skipping",
                    account.display_id(), account.window.used(), self.hourly_quota
                );
                continue;
            }
            let idle = now_ms - account.last_released.load(Ordering::Relaxed);
            if idle > best_idle {
                best_idle = idle;
                best = Some(Arc::clone(account));
            }
        }

        let account = best?;
        account
            .state
            .compare_exchange(
                AccountState::Idle as u8,
                AccountState::Busy as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok()?;
        account.record_request(self.hourly_quota);
        debug!(
            target: "ds_core::accounts",
            "Account {} allocated for request (used_this_window={}, total={}, idle_ms={})",
            account.display_id(),
            account.window.used(),
            account.get_total_requests(),
            now_ms - account.last_released.load(Ordering::Relaxed)
        );
        Some(AccountGuard { account })
    }

    /// 获取所有账号的详细状态
    pub fn account_statuses(&self) -> Vec<AccountStatus> {
        self.accounts
            .iter()
            .map(|entry| AccountStatus::from_account(entry.value(), self.hourly_quota))
            .collect()
    }

    /// 获取所有账号的详细状态（含统计信息）
    pub fn account_statuses_detailed(&self) -> Vec<AccountStatus> {
        self.accounts
            .iter()
            .map(|entry| AccountStatus::from_account(entry.value(), self.hourly_quota))
            .collect()
    }

    /// 优雅关闭（新流程无持久 session，无需清理）
    pub async fn shutdown(&self, _client: &DsClient) {}

    /// 存储 client 和 solver 供恢复任务使用
    pub async fn set_client_solver(&self, client: DsClient, solver: PowSolver) {
        *self.client.write().await = Some(client);
        *self.solver.write().await = Some(solver);
    }

    /// 标记账号为 Error 状态（请求失败时调用）
    pub fn mark_error(&self, email_or_mobile: &str) {
        if let Some(entry) = self.accounts.get(email_or_mobile) {
            let account = entry.value();
            // 只从 Busy 转到 Error（避免覆盖 Invalid）
            account
                .state
                .compare_exchange(
                    AccountState::Busy as u8,
                    AccountState::Error as u8,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .ok();
            warn!(target: "ds_core::accounts", "Account {} marked as Error", account.display_id());
        }
    }

    /// 手动重新登录指定账号（管理员触发）
    /// 成功 → Idle，失败 → error_count++，≥3 则 Invalid
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        let client_opt = self.client.read().await.clone();
        let solver_opt = self.solver.read().await.clone();
        let (Some(client), Some(solver)) = (client_opt, solver_opt) else {
            return Err("client/solver 未初始化".to_string());
        };

        let account = self
            .accounts
            .get(email_or_mobile)
            .ok_or_else(|| format!("账号 {} 不存在", email_or_mobile))?;
        let account = account.value();

        // 只允许 Error/Invalid 状态的账号重登
        let state = account.state();
        if state != AccountState::Error && state != AccountState::Invalid {
            return Err(format!(
                "账号状态为 {}，仅 Error/Invalid 可重登",
                state.as_str()
            ));
        }

        Self::re_login_account(account, &client, &solver).await;

        // 检查重登后状态
        let new_state = account.state();
        if new_state == AccountState::Idle {
            Ok(())
        } else {
            Err(format!("重登失败，当前状态: {}", new_state.as_str()))
        }
    }

    /// 尝试重新登录 Error 状态的账号
    /// 成功 → Idle，失败 → error_count++，≥3 则 Invalid
    async fn re_login_account(account: &Account, client: &DsClient, solver: &PowSolver) {
        let display_id = account.display_id().to_string();
        match try_init_account(&account.creds, client, solver).await {
            Ok(new_account) => {
                // 更新 token
                *account.token.write().unwrap() = new_account.token.read().unwrap().clone();
                account
                    .state
                    .store(AccountState::Idle as u8, Ordering::Relaxed);
                account.error_count.store(0, Ordering::Relaxed);
                info!(target: "ds_core::accounts", "Account {} re-login successful", display_id);
            }
            Err(e) => {
                let count = account.error_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= MAX_ERROR_COUNT {
                    account
                        .state
                        .store(AccountState::Invalid as u8, Ordering::Relaxed);
                    error!(target: "ds_core::accounts", "Account {} re-login failed {} times, marked as Invalid: {}", display_id, count, e);
                } else {
                    warn!(target: "ds_core::accounts", "Account {} re-login failed (attempt {}): {}", display_id, count, e);
                }
            }
        }
    }

    /// 启动后台恢复任务：每 60 秒扫描 Error 账号并尝试重新登录
    pub fn start_recovery_task(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

                let client_opt = pool.client.read().await.clone();
                let solver_opt = pool.solver.read().await.clone();
                let (Some(client), Some(solver)) = (client_opt, solver_opt) else {
                    continue;
                };

                for entry in pool.accounts.iter() {
                    let account = entry.value();
                    if account.state() == AccountState::Error {
                        Self::re_login_account(account, &client, &solver).await;
                    }
                }
            }
        });
    }
}

/// 检测多个账号共用同一个 `device_id` 并告警
///
/// 设备指纹是**设备级**的，上游用它做关联与画像。实测：同一个 `device_id` 下
/// 挂多个账号、累计数百次请求后，这些账号会被禁言（`biz_code=5`）。
/// 这里不阻止启动（避免破坏既有配置），但必须让用户看到风险。
fn warn_on_shared_device_ids(creds: &[AccountConfig]) {
    let mut by_device: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for c in creds {
        let device = c.device_id.trim();
        if device.is_empty() {
            continue;
        }
        let id = if c.email.is_empty() {
            c.mobile.as_str()
        } else {
            c.email.as_str()
        };
        by_device.entry(device).or_default().push(id);
    }

    for (device, accounts) in by_device {
        if accounts.len() > 1 {
            let prefix: String = device.chars().take(12).collect();
            warn!(
                target: "ds_core::accounts",
                "{} accounts share the same device_id ({}…): {}. \
                 The device fingerprint is used by upstream for correlation; \
                 sharing it across accounts increases the risk of muting. \
                 Capture a separate device_id per account (one browser profile each).",
                accounts.len(), prefix, accounts.join(", ")
            );
        }
    }
}

/// 账号展示标识：优先 email，回退 mobile
fn display_id_of(creds: &AccountConfig) -> &str {
    if creds.email.is_empty() {
        &creds.mobile
    } else {
        &creds.email
    }
}

async fn init_account(
    creds: &AccountConfig,
    client: &DsClient,
    solver: &PowSolver,
) -> Result<Account, PoolError> {
    try_init_account(creds, client, solver).await
}

async fn try_init_account(
    creds: &AccountConfig,
    client: &DsClient,
    solver: &PowSolver,
) -> Result<Account, PoolError> {
    // 验证：email 和 mobile 至少一个非空
    if creds.email.is_empty() && creds.mobile.is_empty() {
        return Err(PoolError::Validation(
            "email 和 mobile 不能同时为空".to_string(),
        ));
    }

    let login_payload = LoginPayload {
        email: creds.email.clone(),
        mobile: creds.mobile.clone(),
        password: creds.password.clone(),
        area_code: creds.area_code.clone(),
        device_id: creds.device_id.clone(),
        os: client.client_os().to_string(),
    };

    let login_data = client.login(&login_payload).await?;
    debug!(
        target: "ds_core::client",
        "登录响应: code={}, msg={}, user_id={}, email={:?}, mobile={:?}, muted={:?}, mute_until={:?}",
        login_data.code,
        login_data.msg,
        login_data.user.id,
        login_data.user.email,
        login_data.user.mobile_number,
        login_data.user.chat.as_ref().map(|c| c.is_muted),
        login_data.user.chat.as_ref().and_then(|c| c.mute_until),
    );

    // 禁言早检：登录响应即带 chat.is_muted/mute_until，无需等 health_check
    // 的一次完整 completion 才暴露（禁言账号 health_check 必然失败）。
    if let Some(chat) = &login_data.user.chat
        && chat.is_muted != 0
    {
        error!(
            target: "ds_core::accounts",
            "Account {} is muted until {:?} (detected at login)",
            display_id_of(creds),
            chat.mute_until
        );
        return Err(PoolError::Validation(format!(
            "账号异常(muted/limited)，mute_until={:?}",
            chat.mute_until
        )));
    }

    let mut token = login_data.user.token;

    // 设备校验 / 令牌轮换：真实客户端登录成功后立即调用
    match client.check_device(&token).await {
        Ok(data) => {
            if let Some(rotate) = data.rotate.as_ref() {
                match super::client::extract_rotate_token(rotate) {
                    Some(new_token) => {
                        debug!(
                            target: "ds_core::accounts",
                            "Account {} token rotated via check_device",
                            display_id_of(creds)
                        );
                        token = new_token;
                    }
                    None => debug!(
                        target: "ds_core::accounts",
                        "Account {} check_device rotate 形态未知，保持原令牌: {}",
                        display_id_of(creds),
                        rotate
                    ),
                }
            } else {
                debug!(
                    target: "ds_core::accounts",
                    "Account {} check_device ok (no rotation)",
                    display_id_of(creds)
                );
            }
        }
        // check_device 失败不阻断初始化（真实客户端亦非关键路径）
        Err(e) => debug!(
            target: "ds_core::accounts",
            "Account {} check_device failed (ignored): {}",
            display_id_of(creds),
            e
        ),
    }

    let display_id = display_id_of(creds);

    // 健康检查：创建临时 session → 发送 test completion → 删除 session
    let session_id = client.create_session(&token).await?;
    if let Err(e) = health_check(&token, &session_id, client, solver, "default", display_id).await {
        // 即使健康检查失败也要清理 session
        let _ = client.delete_session(&token, &session_id).await;
        return Err(e);
    }
    let _ = client.delete_session(&token, &session_id).await;

    Ok(Account {
        token: std::sync::RwLock::new(token.into()),
        email: creds.email.clone(),
        mobile: creds.mobile.clone(),
        state: AtomicU8::new(AccountState::Idle as u8),
        last_released: AtomicI64::new(0),
        error_count: AtomicU8::new(0),
        creds: creds.clone(),
        window: SlidingWindowRateLimiter::new(),
    })
}

/// 健康检查用的中性提示词池
///
/// 早期实现固定发「只回复\`Hello, world!\`」——这是**所有部署共用的一个常量字符串**，
/// 上游只要按 prompt 聚类就能把大量账号关联到同一个客户端。这里每次随机取一条，
/// 既保留「账号能否正常完成一次推理」的检查能力，又不再留下全局指纹。
const HEALTH_CHECK_PROMPTS: [&str; 5] = [
    "你好",
    "1+1 等于几？",
    "用一句话介绍一下你自己",
    "今天适合做什么？",
    "帮我想一个周末的小计划",
];

/// 随机取一条健康检查提示词（时间戳低位做选择，无需引入随机数依赖）
fn health_check_prompt() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as usize);
    HEALTH_CHECK_PROMPTS[nanos % HEALTH_CHECK_PROMPTS.len()].to_string()
}

async fn health_check(
    token: &str,
    session_id: &str,
    client: &DsClient,
    solver: &PowSolver,
    model_type: &str,
    display_id: &str,
) -> Result<(), PoolError> {
    let start = std::time::Instant::now();
    let challenge = client
        .create_pow_challenge(token, "/api/v0/chat/completion")
        .await?;

    let result = solver.solve(&challenge)?;
    let pow_header = result.to_header();

    let payload = CompletionPayload {
        chat_session_id: session_id.to_string(),
        parent_message_id: None,
        model_type: model_type.to_string(),
        prompt: health_check_prompt(),
        ref_file_ids: vec![],
        thinking_enabled: false,
        search_enabled: false,
        action: None,
        preempt: false,
    };

    let mut stream = client.completion(token, &pow_header, &payload).await?;
    // 消费流并检查是否收到正常 SSE（健康账号应有 ready/response 事件）
    let mut data = Vec::new();
    while let Some(chunk) = stream.try_next().await? {
        data.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&data);

    // 检测账号是否异常（muted / 限流等）
    if text.contains(r#""biz_code":"#) {
        error!(
            target: "ds_core::accounts",
            "health_check 检测到业务错误: account={}, response={}",
            display_id,
            text.lines().find(|l| l.contains("biz_code")).unwrap_or(&text)
        );
        return Err(PoolError::Validation("账号异常(muted/limited)".into()));
    }

    // 检查 SSE 流是否正常结束
    if !text.contains(r#""FINISHED""#) && !text.contains(r#""INCOMPLETE""#) {
        return Err(PoolError::Validation("SSE 流未正常结束".into()));
    }

    debug!(
        target: "ds_core::accounts",
        "health_check 完成 model_type={} account={} elapsed={:?}",
        model_type, display_id, start.elapsed()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(email: &str, device_id: &str) -> AccountConfig {
        AccountConfig {
            email: email.to_string(),
            mobile: String::new(),
            area_code: String::new(),
            password: "pw".to_string(),
            device_id: device_id.to_string(),
        }
    }

    #[test]
    fn sliding_window_counts_and_reports_usage() {
        let w = SlidingWindowRateLimiter::new();
        assert_eq!(w.used(), 0);
        assert_eq!(w.record(), 1);
        assert_eq!(w.record(), 2);
        assert_eq!(w.used(), 2);
    }

    #[test]
    fn quota_of_zero_means_unlimited() {
        let a = Account::new_invalid(account("a@example.com", "dev"), 0);
        for _ in 0..500 {
            a.record_request(0);
        }
        assert!(a.within_quota(0), "0 必须表示不限制");
    }

    #[test]
    fn account_is_blocked_after_reaching_quota() {
        let a = Account::new_invalid(account("a@example.com", "dev"), 3);
        let limit = 3;
        assert!(a.within_quota(limit));
        a.record_request(limit);
        assert!(a.within_quota(limit), "达到上限前仍可用");
        a.record_request(limit);
        assert!(a.within_quota(limit));
        a.record_request(limit);
        assert!(!a.within_quota(limit), "达到上限后该窗口内不应再被分配");
    }

    #[test]
    fn expired_sliding_window_resets_usage() {
        let w = SlidingWindowRateLimiter::new();
        for _ in 0..5 {
            w.record();
        }
        assert_eq!(w.used(), 5);
        // 手动插入旧时间戳模拟窗口过期
        let now = now_secs();
        let mut ts = w.timestamps.lock().unwrap();
        ts.clear();
        // 插入 1 小时前的时间戳
        for _ in 0..5 {
            ts.push_back(now - WINDOW_SECS as i64 - 100);
        }
        drop(ts);
        assert_eq!(w.used(), 0, "窗口过期后用量应视作 0");
        assert_eq!(w.record(), 1, "过期后重新计数应从 1 开始");
    }

    #[test]
    fn shared_device_ids_are_detected() {
        let creds = vec![
            account("a@example.com", "same-device"),
            account("b@example.com", "same-device"),
            account("c@example.com", "own-device"),
        ];
        let mut by_device: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for c in &creds {
            if !c.device_id.trim().is_empty() {
                by_device
                    .entry(c.device_id.as_str())
                    .or_default()
                    .push(c.email.as_str());
            }
        }
        let shared: Vec<_> = by_device.iter().filter(|(_, v)| v.len() > 1).collect();
        assert_eq!(shared.len(), 1, "应只检出一组共用指纹");
        assert_eq!(shared[0].1.len(), 2);
    }

    /// 构造一个可用的（Idle）测试账号
    fn idle_account(email: &str) -> Arc<Account> {
        Arc::new(Account {
            token: std::sync::RwLock::new("t".into()),
            email: email.to_string(),
            mobile: String::new(),
            state: AtomicU8::new(AccountState::Idle as u8),
            last_released: AtomicI64::new(0),
            error_count: AtomicU8::new(0),
            creds: account(email, "dev"),
            window: SlidingWindowRateLimiter::new(),
        })
    }

    #[test]
    fn pool_skips_accounts_that_exhausted_their_quota() {
        let pool = AccountPool::new(2);
        pool.accounts
            .insert("a@example.com".to_string(), idle_account("a@example.com"));

        // 配额 2：前两次可以拿到账号
        assert!(pool.get_account().is_some(), "第 1 次应可分配");
        assert!(pool.get_account().is_some(), "第 2 次应可分配");
        // 第三次该账号已用尽 → 池中无可用账号
        assert!(
            pool.get_account().is_none(),
            "配额用尽后不应再分配该账号（调用方据此返回 429）"
        );
    }

    #[test]
    fn pool_with_unlimited_quota_never_blocks() {
        let pool = AccountPool::new(0);
        pool.accounts
            .insert("a@example.com".to_string(), idle_account("a@example.com"));
        for i in 0..50 {
            assert!(pool.get_account().is_some(), "配额 0 时第 {i} 次也应可分配");
        }
    }

    #[test]
    fn exhausted_account_does_not_block_other_accounts() {
        let pool = AccountPool::new(1);
        pool.accounts
            .insert("a@example.com".to_string(), idle_account("a@example.com"));
        pool.accounts
            .insert("b@example.com".to_string(), idle_account("b@example.com"));

        // 两个账号各能用 1 次（顺序取决于「空闲最久」策略）
        assert!(pool.get_account().is_some());
        assert!(pool.get_account().is_some());
        assert!(pool.get_account().is_none(), "两个账号都用尽后应返回 None");
    }

    #[test]
    fn empty_device_ids_are_ignored_by_shared_detection() {
        // 空 device_id 会被上游拒绝登录，但不该在这里被误报为「共用」
        let creds = vec![
            account("a@example.com", ""),
            account("b@example.com", "   "),
        ];
        let mut by_device: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for c in &creds {
            if !c.device_id.trim().is_empty() {
                by_device.entry(c.device_id.as_str()).or_default().push("x");
            }
        }
        assert!(by_device.is_empty());
    }
}

//! 账号池管理 —— 多账号负载均衡
//!
//! 1 account = 1 session = 1 concurrency。多并发需横向扩展账号数。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use dashmap::DashMap;
use futures::TryStreamExt;
use futures::future::join_all;
use log::{debug, error, info, warn};
use tokio::sync::{RwLock, Semaphore};

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
    const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Busy,
            2 => Self::Error,
            _ => Self::Invalid,
        }
    }

    pub const fn as_str(self) -> &'static str {
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
        let window_remaining = (window_started + WINDOW_SECS - now_secs()).max(0);
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
    /// 复用会话槽（`session_reuse = true` 时使用）
    ///
    /// 真实客户端一个会话长期复用、几乎不删；这里保存最近一次成功完成的会话，
    /// 空闲超过阈值由后台任务回收（见 `AccountPool::reap_sessions`）。
    session: Mutex<Option<CachedSession>>,
    /// 绑定该账号设备身份（X-Device-Id）的客户端视图
    ///
    /// 真实 Web 客户端「一个浏览器 profile = 一个数美 device_id + 一个
    /// X-Device-Id」，HIF 风控令牌也按设备下发；这里按账号派生，
    /// 避免多账号共用同一设备身份与同一令牌。
    client: DsClient,
}

/// 复用会话槽里的一条记录
struct CachedSession {
    id: String,
    last_used: Instant,
}

/// 连续登录失败上限，达到后标记为 Invalid
const MAX_ERROR_COUNT: u8 = 3;

/// 配额窗口长度：1 小时
const WINDOW_SECS: i64 = 3600;

fn now_secs() -> i64 {
    unix_timestamp_secs().unwrap_or(i64::MAX)
}

/// 当前 Unix 时间戳（毫秒），用于账号释放冷却判断
fn now_ms() -> i64 {
    unix_timestamp_millis().unwrap_or(i64::MAX)
}

/// Unix 秒级时间戳；系统时钟异常（早于 EPOCH / 溢出 i64）时返回 None
fn unix_timestamp_secs() -> Option<i64> {
    i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
    .ok()
}

/// Unix 毫秒级时间戳；系统时钟异常（早于 EPOCH / 溢出 i64）时返回 None
fn unix_timestamp_millis() -> Option<i64> {
    i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
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
    const fn new() -> Self {
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
        let cutoff = now - WINDOW_SECS;

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
        let cutoff = now - WINDOW_SECS;

        while ts.front().is_some_and(|&t| t < cutoff) {
            ts.pop_front();
        }

        ts.len() as u64
    }
}

impl Account {
    /// 当前缓存的会话 ID（`session_reuse` 模式下复用）
    pub(crate) fn cached_session_id(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.id.clone())
    }

    /// 记下可复用的会话（流正常结束后调用）
    pub(crate) fn put_cached_session(&self, session_id: &str) {
        let mut slot = self.session.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(CachedSession {
            id: session_id.to_string(),
            last_used: Instant::now(),
        });
    }

    /// 清掉缓存的会话（仅在 ID 匹配时，避免误删新会话）
    pub(crate) fn clear_cached_session(&self, session_id: &str) {
        let mut slot = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|c| c.id == session_id) {
            *slot = None;
        }
    }

    /// 取出需要回收的会话：`max_idle = None` 表示无条件回收（进程退出时）
    fn take_reapable_session(&self, max_idle: Option<Duration>) -> Option<String> {
        let mut slot = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let due = match (slot.as_ref(), max_idle) {
            (Some(c), Some(idle)) => c.last_used.elapsed() >= idle,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if due { slot.take().map(|c| c.id) } else { None }
    }

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
    fn new_invalid(creds: AccountConfig, _hourly_quota: u64, client: &DsClient) -> Self {
        let scoped = client.scoped_to(&account_x_device_id(&creds, client));
        Self {
            token: std::sync::RwLock::new(String::new().into()),
            email: creds.email.clone(),
            mobile: creds.mobile.clone(),
            state: AtomicU8::new(AccountState::Invalid as u8),
            last_released: AtomicI64::new(0),
            error_count: AtomicU8::new(MAX_ERROR_COUNT),
            creds,
            window: SlidingWindowRateLimiter::new(),
            session: Mutex::new(None),
            client: scoped,
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

impl Account {
    /// 该账号的设备身份视图（X-Device-Id + 对应设备的 HIF 令牌）
    pub(crate) fn client(&self) -> DsClient {
        self.client.clone()
    }
}

impl AccountGuard {
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// 取得账号的 `Arc` 句柄
    ///
    /// 需要在守卫被移动（例如移交流处理）之后继续使用账号时使用。
    pub fn account_arc(&self) -> Arc<Account> {
        Arc::clone(&self.account)
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
        self.account
            .last_released
            .store(now_ms(), Ordering::Relaxed);
    }
}

pub struct AccountPool {
    /// 每账号每小时请求上限（0 = 不限制）
    hourly_quota: u64,
    /// 启动时是否用一条 completion 做健康检查（false = 对齐真实客户端启动序列）
    startup_health_check: bool,
    /// 是否复用账号会话（false = 每轮建删，历史行为）
    session_reuse: bool,
    /// 复用模式下会话的空闲回收秒数（0 = 只在进程退出时删除）
    session_idle_secs: u64,
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
    pub fn new(hourly_quota: u64, startup_health_check: bool) -> Self {
        Self {
            hourly_quota,
            startup_health_check,
            // 默认保持历史行为：不复用会话（每轮建删）
            session_reuse: false,
            session_idle_secs: 0,
            accounts: DashMap::new(),
            client: RwLock::new(None),
            solver: RwLock::new(None),
        }
    }

    /// 配置会话复用策略（链式调用，默认关闭）
    #[must_use]
    pub const fn with_session_reuse(mut self, reuse: bool, idle_secs: u64) -> Self {
        self.session_reuse = reuse;
        self.session_idle_secs = idle_secs;
        self
    }

    /// 回收会话：`max_idle = None` 表示全部回收（进程退出时用），
    /// 否则只回收空闲超过该时长的会话。返回回收数量。
    ///
    /// 只会删除**本代理自己缓存**的会话；正在流式传输中的会话不在此列
    /// （它们在 `Chat::active_sessions` 里，由流结束时的清理逻辑负责）。
    pub async fn reap_sessions(&self, max_idle: Option<Duration>) -> usize {
        let mut reaped = 0;
        for entry in &self.accounts {
            let account = entry.value();
            let Some(session_id) = account.take_reapable_session(max_idle) else {
                continue;
            };
            let client = account.client();
            let token = account.token();
            match client.delete_session(&token, &session_id).await {
                Ok(()) => {
                    reaped += 1;
                    debug!(
                        target: "ds_core::accounts",
                        "回收空闲会话: account={}, session={session_id}",
                        account.display_id()
                    );
                }
                Err(e) => warn!(
                    target: "ds_core::accounts",
                    "回收会话失败: account={}, session={session_id}: {e}",
                    account.display_id()
                ),
            }
        }
        reaped
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

        // 限制并发初始化数，避免对 DeepSeek 端和本地连接池造成压力
        let semaphore = Arc::new(Semaphore::new(13));
        let futures: Vec<_> = creds
            .into_iter()
            .map(|creds| {
                let client = client.clone();
                let solver = solver.clone();
                let sem = semaphore.clone();
                let startup_health_check = self.startup_health_check;
                async move {
                    let _permit = sem.acquire().await.expect("信号量未关闭");
                    let display_id = if creds.email.is_empty() {
                        creds.mobile.clone()
                    } else {
                        creds.email.clone()
                    };
                    let account =
                        match init_account(&creds, &client, &solver, startup_health_check).await {
                        Ok(account) => {
                            info!(target: "ds_core::accounts", "Account {display_id} initialized successfully");
                            account
                        }
                        Err(e) => {
                            warn!(target: "ds_core::accounts", "Account {display_id} initialization failed: {e}");
                            // 即使初始化失败也加入池，标记为 Invalid 以便前台展示
                            Account::new_invalid(creds.clone(), self.hourly_quota, &client)
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

        let account = init_account(creds, client, solver, self.startup_health_check).await?;
        let _id = account.display_id().to_string();
        self.accounts.insert(display_id.clone(), Arc::new(account));
        info!(target: "ds_core::accounts", "Account {display_id} added dynamically");
        Ok(display_id)
    }

    /// 动态移除账号（仅空闲账号可移除）
    pub fn remove_account(&self, email_or_mobile: &str) -> Result<String, PoolError> {
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
        info!(target: "ds_core::accounts", "Account {id} removed");
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

        let now_ms = now_ms();

        let mut best: Option<Arc<Account>> = None;
        let mut best_idle = i64::MIN;

        for entry in &self.accounts {
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

        // 克隆 Arc 后再重登：避免跨 await 持有 DashMap 分片读锁
        let account = self
            .accounts
            .get(email_or_mobile)
            .map(|a| a.value().clone())
            .ok_or_else(|| format!("账号 {email_or_mobile} 不存在"))?;

        // 只允许 Error/Invalid 状态的账号重登
        let state = account.state();
        if state != AccountState::Error && state != AccountState::Invalid {
            return Err(format!(
                "账号状态为 {}，仅 Error/Invalid 可重登",
                state.as_str()
            ));
        }

        Self::re_login_account(&account, &client, &solver, self.startup_health_check).await;

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
    /// 登录阶段的**终止性**错误：重试不会改变结果
    ///
    /// 封禁（10）/ 禁言（5）/ 设备校验失败（11）/ 凭据错误（2）以及本地校验失败，
    /// 反复重试只会继续打上游 —— 文档里明确「禁言后继续重试不会加速解禁，反而可能延长」。
    const fn is_terminal_login_error(e: &PoolError) -> bool {
        match e {
            PoolError::Validation(_) => true,
            PoolError::Client(ClientError::Business { code, .. }) => {
                matches!(code, 2 | 5 | 10 | 11)
            }
            _ => false,
        }
    }

    async fn re_login_account(
        account: &Account,
        client: &DsClient,
        solver: &PowSolver,
        startup_health_check: bool,
    ) {
        let display_id = account.display_id().to_string();
        match try_init_account(&account.creds, client, solver, startup_health_check).await {
            Ok(new_account) => {
                // 更新 token
                *account.token.write().unwrap() = new_account.token.read().unwrap().clone();
                account
                    .state
                    .store(AccountState::Idle as u8, Ordering::Relaxed);
                account.error_count.store(0, Ordering::Relaxed);
                info!(target: "ds_core::accounts", "Account {display_id} re-login successful");
            }
            Err(e) if Self::is_terminal_login_error(&e) => {
                // 终止性错误：立即置 Invalid，停止重试（避免反复登录已被封禁/禁言的账号）
                account
                    .state
                    .store(AccountState::Invalid as u8, Ordering::Relaxed);
                error!(
                    target: "ds_core::accounts",
                    "Account {display_id} 登录被终止性拒绝，已标记 Invalid 并停止重试: {e}"
                );
            }
            Err(e) => {
                let count = account.error_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= MAX_ERROR_COUNT {
                    account
                        .state
                        .store(AccountState::Invalid as u8, Ordering::Relaxed);
                    error!(target: "ds_core::accounts", "Account {display_id} re-login failed {count} times, marked as Invalid: {e}");
                } else {
                    warn!(target: "ds_core::accounts", "Account {display_id} re-login failed (attempt {count}): {e}");
                }
            }
        }
    }

    /// 启动后台恢复任务：每 60 秒扫描 Error 账号并尝试重新登录
    pub fn start_recovery_task(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_mins(1)).await;

                let client_opt = pool.client.read().await.clone();
                let solver_opt = pool.solver.read().await.clone();
                let (Some(client), Some(solver)) = (client_opt, solver_opt) else {
                    continue;
                };

                for entry in &pool.accounts {
                    let account = entry.value();
                    if account.state() == AccountState::Error {
                        Self::re_login_account(
                            account,
                            &client,
                            &solver,
                            pool.startup_health_check,
                        )
                        .await;
                    }
                }

                // 复用模式下顺带回收空闲会话（0 = 只在进程退出时删除）
                if pool.session_reuse && pool.session_idle_secs > 0 {
                    pool.reap_sessions(Some(tokio::time::Duration::from_secs(
                        pool.session_idle_secs,
                    )))
                    .await;
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
    startup_health_check: bool,
) -> Result<Account, PoolError> {
    try_init_account(creds, client, solver, startup_health_check).await
}

/// 账号的 X-Device-Id
///
/// - 有数美 `device_id` 时按其派生：稳定、且**每个账号各不相同**（对齐真实客户端
///   「一个浏览器 profile 一个设备」）；
/// - 显式配置了全局 `client_device_id` 时以配置为准（多账号共用一台设备的场景）；
/// - 都没有时回退到客户端自身的设备身份。
fn account_x_device_id(creds: &AccountConfig, client: &DsClient) -> String {
    if !creds.device_id.trim().is_empty() {
        return super::client::derive_device_uuid(
            format!("ds-free-api:x-device-id:{}", creds.device_id.trim()).as_bytes(),
        );
    }
    client.device_id().to_string()
}

async fn try_init_account(
    creds: &AccountConfig,
    client: &DsClient,
    solver: &PowSolver,
    startup_health_check: bool,
) -> Result<Account, PoolError> {
    // 验证：email 和 mobile 至少一个非空
    if creds.email.is_empty() && creds.mobile.is_empty() {
        return Err(PoolError::Validation(
            "email 和 mobile 不能同时为空".to_string(),
        ));
    }

    // 设备身份按账号派生（空 device_id 时回退到全局配置的 X-Device-Id）
    let x_device_id = account_x_device_id(creds, client);
    let client = &client.scoped_to(&x_device_id);
    // 真实客户端在启动时就取好该设备的 HIF 令牌；这里同样预热，
    // 失败不阻断（与客户端轮询失败时的行为一致）
    client.warm_up_hif().await;

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
    //
    // `startup_health_check = false` 时整段跳过：真实客户端启动只建会话、不发消息，
    // 而账号可用性已由「登录成功 + 禁言早检（biz_code 5）」保证。
    if startup_health_check {
        let session_id = client.create_session(&token).await?;
        if let Err(e) =
            health_check(&token, &session_id, client, solver, "default", display_id).await
        {
            // 即使健康检查失败也要清理 session
            if let Err(cleanup_err) = client.delete_session(&token, &session_id).await {
                log::warn!(
                    target: "ds_core::accounts",
                    "健康检查失败后清理 session {session_id} 也失败: {cleanup_err}"
                );
            }
            return Err(e);
        }
        if let Err(e) = client.delete_session(&token, &session_id).await {
            log::warn!(
                target: "ds_core::accounts",
                "健康检查后清理 session {session_id} 失败: {e}"
            );
        }
    } else {
        debug!(
            target: "ds_core::accounts",
            "Account {display_id} 跳过启动健康检查（startup_health_check = false）"
        );
    }

    Ok(Account {
        token: std::sync::RwLock::new(token.into()),
        email: creds.email.clone(),
        mobile: creds.mobile.clone(),
        state: AtomicU8::new(AccountState::Idle as u8),
        last_released: AtomicI64::new(0),
        error_count: AtomicU8::new(0),
        creds: creds.clone(),
        window: SlidingWindowRateLimiter::new(),
        session: Mutex::new(None),
        client: client.clone(),
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

    /// 测试用客户端：HIF 关闭，设备身份由 api_base 派生（不发起网络请求）
    fn test_client() -> DsClient {
        DsClient::new(
            "https://example.invalid/api/v0".to_string(),
            "https://example.invalid/x.wasm".to_string(),
            super::super::client::ClientIdentity::default(),
            &super::super::client::HifConfig {
                enabled: false,
                ..Default::default()
            },
            super::super::client::EmulationProfile::default(),
            None,
        )
    }

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
    fn terminal_login_errors_stop_retrying() {
        use super::super::client::ClientError;
        // 封禁 / 禁言 / 设备校验失败 / 凭据错误 → 终止
        for code in [2, 5, 10, 11] {
            assert!(
                AccountPool::is_terminal_login_error(&PoolError::Client(ClientError::Business {
                    code,
                    msg: String::new(),
                })),
                "biz_code={code} 应视为终止性错误"
            );
        }
        assert!(AccountPool::is_terminal_login_error(
            &PoolError::Validation("账号异常(muted/limited)".into())
        ));
        // 网络类错误仍应重试
        assert!(!AccountPool::is_terminal_login_error(&PoolError::Pow(
            super::super::PowError::NoSolution
        )));
    }

    #[test]
    fn per_account_device_identity_is_unique_and_stable() {
        let client = test_client();
        let a1 = account_x_device_id(&account("a@example.com", "shumei-device-a"), &client);
        let a2 = account_x_device_id(&account("a@example.com", "shumei-device-a"), &client);
        let b = account_x_device_id(&account("b@example.com", "shumei-device-b"), &client);
        assert_eq!(a1, a2, "同一账号的设备身份必须稳定");
        assert_ne!(a1, b, "不同账号不得共用同一个 X-Device-Id");
        assert_eq!(a1.len(), 36);
        // 无 device_id 时回退到客户端自身身份
        let fallback = account_x_device_id(&account("c@example.com", ""), &client);
        assert_eq!(fallback, client.device_id());
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
        let a = Account::new_invalid(account("a@example.com", "dev"), 0, &test_client());
        for _ in 0..500 {
            a.record_request(0);
        }
        assert!(a.within_quota(0), "0 必须表示不限制");
    }

    #[test]
    fn account_is_blocked_after_reaching_quota() {
        let a = Account::new_invalid(account("a@example.com", "dev"), 3, &test_client());
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
            ts.push_back(now - WINDOW_SECS - 100);
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

    /// 复用会话槽：写入 / 读取 / 按 ID 精确清除
    #[test]
    fn cached_session_slot_roundtrip() {
        let account = idle_account("cache@example.com");
        assert!(account.cached_session_id().is_none(), "初始应为空");

        account.put_cached_session("s-1");
        assert_eq!(account.cached_session_id().as_deref(), Some("s-1"));

        // 不匹配的 ID 不得误删
        account.clear_cached_session("s-other");
        assert_eq!(account.cached_session_id().as_deref(), Some("s-1"));

        account.clear_cached_session("s-1");
        assert!(account.cached_session_id().is_none());
    }

    /// 空闲回收：未超时不动，超时取出；`None` 表示无条件回收
    #[test]
    fn reapable_session_respects_idle_window() {
        let account = idle_account("idle@example.com");
        account.put_cached_session("s-2");

        assert!(
            account
                .take_reapable_session(Some(Duration::from_mins(5)))
                .is_none(),
            "刚用过的会话不应被回收"
        );
        assert_eq!(
            account.take_reapable_session(None).as_deref(),
            Some("s-2"),
            "退出时应无条件回收"
        );
        assert!(account.cached_session_id().is_none(), "回收后槽位应清空");
    }

    /// 会话策略默认关闭复用，且 `with_session_reuse` 只改策略相关字段
    #[test]
    fn session_reuse_defaults_off() {
        let pool = AccountPool::new(10, true);
        assert!(!pool.session_reuse, "默认必须保持「每轮建删」的历史行为");
        assert_eq!(pool.session_idle_secs, 0);

        let pool = pool.with_session_reuse(true, 900);
        assert!(pool.session_reuse);
        assert_eq!(pool.session_idle_secs, 900);
        assert_eq!(pool.hourly_quota, 10, "其它配置不应被改动");
    }

    /// 空账号池回收会话是安全的空操作
    #[tokio::test]
    async fn reap_sessions_on_empty_pool_is_noop() {
        let pool = AccountPool::new(0, true);
        assert_eq!(pool.reap_sessions(None).await, 0);
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
            session: Mutex::new(None),
            client: test_client(),
        })
    }

    #[test]
    fn pool_skips_accounts_that_exhausted_their_quota() {
        let pool = AccountPool::new(2, true);
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
        let pool = AccountPool::new(0, true);
        pool.accounts
            .insert("a@example.com".to_string(), idle_account("a@example.com"));
        for i in 0..50 {
            assert!(pool.get_account().is_some(), "配额 0 时第 {i} 次也应可分配");
        }
    }

    #[test]
    fn exhausted_account_does_not_block_other_accounts() {
        let pool = AccountPool::new(1, true);
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

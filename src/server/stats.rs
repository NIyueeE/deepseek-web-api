//! 请求统计 —— 轻量级原子计数器 + 定期持久化 + 按模型拆分

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use serde::Serialize;

use super::mask_prefix;
use super::store::StoreManager;

/// 持久化间隔：每 30 次请求写盘一次
const PERSIST_INTERVAL: u64 = 30;
/// 请求日志最大条数
const LOG_CAPACITY: usize = 200;

/// 单个模型的统计计数器
pub struct ModelStats {
    pub prompt_tokens: AtomicU64,
    pub completion_tokens: AtomicU64,
    pub requests: AtomicU64,
}

impl ModelStats {
    const fn new() -> Self {
        Self {
            prompt_tokens: AtomicU64::new(0),
            completion_tokens: AtomicU64::new(0),
            requests: AtomicU64::new(0),
        }
    }
}

/// 单个 API Key 的统计计数器
pub struct KeyUsage {
    pub prompt_tokens: AtomicU64,
    pub completion_tokens: AtomicU64,
    pub requests: AtomicU64,
}

impl KeyUsage {
    const fn new() -> Self {
        Self {
            prompt_tokens: AtomicU64::new(0),
            completion_tokens: AtomicU64::new(0),
            requests: AtomicU64::new(0),
        }
    }
}

/// 单条请求日志
#[derive(Serialize, Clone)]
pub struct RequestLog {
    pub timestamp: u64,
    pub request_id: String,
    pub model: String,
    pub api_key: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    pub success: bool,
}

/// 请求统计计数器
pub struct Stats {
    pub total_requests: AtomicU64,
    pub success_requests: AtomicU64,
    pub failed_requests: AtomicU64,
    pub total_latency_ms: AtomicU64,
    pub total_prompt_tokens: AtomicU64,
    pub total_completion_tokens: AtomicU64,
    pub start_time: Instant,
    /// 上次持久化时的 total_requests 值
    last_persisted: AtomicU64,
    /// 持久化串行化锁
    ///
    /// `persist_now()` 会 spawn 异步写盘；若不加锁，多次 spawn 可能**乱序完成**，
    /// 导致较旧的快照覆盖较新的（stats.json 回退），而 `write_json_file` 使用固定
    /// 的 `stats.json.tmp` 文件名，并发写还会互相踩踏。
    persist_lock: tokio::sync::Mutex<()>,
    /// 持久化存储
    store: Option<Arc<StoreManager>>,
    /// 按模型拆分的统计
    pub model_stats: DashMap<String, ModelStats>,
    /// 按 API Key 拆分的统计
    pub key_stats: DashMap<String, KeyUsage>,
    /// 请求日志环形缓冲区
    pub request_logs: Mutex<VecDeque<RequestLog>>,
}

impl Stats {
    /// 创建 Stats，可选从持久化数据恢复（含模型/Key 维度统计 + 请求日志）
    pub fn new_with_store(store: Option<Arc<StoreManager>>) -> Self {
        let (
            total_requests,
            success_requests,
            failed_requests,
            prompt_tokens,
            completion_tokens,
            model_stats_data,
            key_stats_data,
            logs_data,
        ) = store.as_ref().map_or_else(
            || (0, 0, 0, 0, 0, HashMap::new(), HashMap::new(), Vec::new()),
            |s| {
                let st = futures::executor::block_on(s.load_stats());
                (
                    st.total_requests,
                    st.success_requests,
                    st.failed_requests,
                    st.total_prompt_tokens,
                    st.total_completion_tokens,
                    st.model_stats,
                    st.key_stats,
                    st.request_logs,
                )
            },
        );

        // 恢复模型统计
        let model_stats: DashMap<String, ModelStats> = DashMap::new();
        for (model, data) in &model_stats_data {
            model_stats.insert(
                model.clone(),
                ModelStats {
                    prompt_tokens: AtomicU64::new(data.prompt_tokens),
                    completion_tokens: AtomicU64::new(data.completion_tokens),
                    requests: AtomicU64::new(data.requests),
                },
            );
        }

        // 恢复 Key 统计
        let key_stats: DashMap<String, KeyUsage> = DashMap::new();
        for (key, data) in &key_stats_data {
            key_stats.insert(
                key.clone(),
                KeyUsage {
                    prompt_tokens: AtomicU64::new(data.prompt_tokens),
                    completion_tokens: AtomicU64::new(data.completion_tokens),
                    requests: AtomicU64::new(data.requests),
                },
            );
        }

        // 恢复请求日志（最多 LOG_CAPACITY 条）
        let mut logs = VecDeque::with_capacity(LOG_CAPACITY);
        for entry in logs_data.iter().rev().take(LOG_CAPACITY).rev() {
            logs.push_back(super::stats::RequestLog {
                timestamp: entry.timestamp,
                request_id: entry.request_id.clone(),
                model: entry.model.clone(),
                api_key: entry.api_key.clone(),
                prompt_tokens: entry.prompt_tokens,
                completion_tokens: entry.completion_tokens,
                latency_ms: entry.latency_ms,
                success: entry.success,
            });
        }

        Self {
            total_requests: AtomicU64::new(total_requests),
            success_requests: AtomicU64::new(success_requests),
            failed_requests: AtomicU64::new(failed_requests),
            total_latency_ms: AtomicU64::new(0), // latency 不持久化
            total_prompt_tokens: AtomicU64::new(prompt_tokens),
            total_completion_tokens: AtomicU64::new(completion_tokens),
            start_time: Instant::now(),
            last_persisted: AtomicU64::new(total_requests),
            persist_lock: tokio::sync::Mutex::new(()),
            store,
            model_stats,
            key_stats,
            request_logs: Mutex::new(logs),
        }
    }

    /// 追加请求日志
    pub fn append_log(&self, log: RequestLog) {
        let mut logs = self.request_logs.lock().unwrap();
        if logs.len() >= LOG_CAPACITY {
            logs.pop_front();
        }
        logs.push_back(log);
    }

    /// 获取最近的请求日志
    pub fn recent_logs(&self, limit: usize) -> Vec<RequestLog> {
        let logs = self.request_logs.lock().unwrap();
        logs.iter().rev().take(limit).cloned().collect()
    }

    /// 记录 token 用量（含模型 + API Key 维度）
    pub fn record_tokens_for_model_and_key(
        &self,
        model: &str,
        api_key: Option<&str>,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        self.total_prompt_tokens
            .fetch_add(prompt_tokens, Ordering::Relaxed);
        self.total_completion_tokens
            .fetch_add(completion_tokens, Ordering::Relaxed);
        // 按模型记录
        let ms = self
            .model_stats
            .entry(model.to_string())
            .or_insert_with(ModelStats::new);
        ms.prompt_tokens.fetch_add(prompt_tokens, Ordering::Relaxed);
        ms.completion_tokens
            .fetch_add(completion_tokens, Ordering::Relaxed);
        ms.requests.fetch_add(1, Ordering::Relaxed);
        drop(ms);
        // 按 API Key 记录
        if let Some(key) = api_key {
            let ku = self
                .key_stats
                .entry(key.to_string())
                .or_insert_with(KeyUsage::new);
            ku.prompt_tokens.fetch_add(prompt_tokens, Ordering::Relaxed);
            ku.completion_tokens
                .fetch_add(completion_tokens, Ordering::Relaxed);
            ku.requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 记录一次请求完成
    pub fn record_request(self: &Arc<Self>, success: bool, latency_ms: u64) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        if success {
            self.success_requests.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_requests.fetch_add(1, Ordering::Relaxed);
        }
        self.total_latency_ms
            .fetch_add(latency_ms, Ordering::Relaxed);
        self.maybe_persist();
    }

    /// 检查是否需要持久化
    fn maybe_persist(self: &Arc<Self>) {
        let total = self.total_requests.load(Ordering::Relaxed);
        let last = self.last_persisted.load(Ordering::Relaxed);
        if total - last >= PERSIST_INTERVAL
            && self
                .last_persisted
                .compare_exchange(last, total, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.persist_now();
        }
    }

    /// 立即持久化当前统计（含模型/Key 维度 + 请求日志）
    ///
    /// 串行化并「写入时取快照」：
    /// - 多个并发调用按锁顺序执行，不会乱序覆盖（否则 stats.json 会回退到旧值）
    /// - 快照在**持锁后**读取，因此最后一次写入必定是最新数据
    /// - `write_json_file` 的 `stats.json.tmp` 也不会被并发写踩踏
    pub fn persist_now(self: &Arc<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let stats = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = stats.persist_lock.lock().await;

            let model_stats: HashMap<String, super::store::ModelStatsData> = stats
                .model_stats
                .iter()
                .map(|r| {
                    (
                        r.key().clone(),
                        super::store::ModelStatsData {
                            prompt_tokens: r.value().prompt_tokens.load(Ordering::Relaxed),
                            completion_tokens: r.value().completion_tokens.load(Ordering::Relaxed),
                            requests: r.value().requests.load(Ordering::Relaxed),
                        },
                    )
                })
                .collect();
            let key_stats: HashMap<String, super::store::KeyStatsData> = stats
                .key_stats
                .iter()
                .map(|r| {
                    let masked = mask_prefix(r.key(), 8);
                    (
                        masked,
                        super::store::KeyStatsData {
                            prompt_tokens: r.value().prompt_tokens.load(Ordering::Relaxed),
                            completion_tokens: r.value().completion_tokens.load(Ordering::Relaxed),
                            requests: r.value().requests.load(Ordering::Relaxed),
                        },
                    )
                })
                .collect();
            let logs = {
                // 作用域内取锁，确保 std guard 不跨 await
                let guard = stats.request_logs.lock().unwrap();
                guard
                    .iter()
                    .map(|l| super::store::RequestLogData {
                        timestamp: l.timestamp,
                        request_id: l.request_id.clone(),
                        model: l.model.clone(),
                        api_key: l.api_key.clone(),
                        prompt_tokens: l.prompt_tokens,
                        completion_tokens: l.completion_tokens,
                        latency_ms: l.latency_ms,
                        success: l.success,
                    })
                    .collect()
            };
            let st = super::store::StatsStore {
                total_requests: stats.total_requests.load(Ordering::Relaxed),
                success_requests: stats.success_requests.load(Ordering::Relaxed),
                failed_requests: stats.failed_requests.load(Ordering::Relaxed),
                total_prompt_tokens: stats.total_prompt_tokens.load(Ordering::Relaxed),
                total_completion_tokens: stats.total_completion_tokens.load(Ordering::Relaxed),
                model_stats,
                key_stats,
                request_logs: logs,
            };
            if let Err(e) = store.save_stats(&st).await {
                log::warn!(target: "stats", "persist failed: {e}");
            }
        });
    }

    /// 生成统计快照
    pub fn snapshot(&self) -> StatsSnapshot {
        let total = self.total_requests.load(Ordering::Relaxed);
        let success = self.success_requests.load(Ordering::Relaxed);
        let failed = self.failed_requests.load(Ordering::Relaxed);
        let total_latency = self.total_latency_ms.load(Ordering::Relaxed);
        let uptime_secs = self.start_time.elapsed().as_secs();

        let prompt_tokens = self.total_prompt_tokens.load(Ordering::Relaxed);
        let completion_tokens = self.total_completion_tokens.load(Ordering::Relaxed);

        StatsSnapshot {
            total_requests: total,
            success_requests: success,
            failed_requests: failed,
            avg_latency_ms: total_latency.checked_div(total).unwrap_or(0),
            total_prompt_tokens: prompt_tokens,
            total_completion_tokens: completion_tokens,
            uptime_secs,
            models: self
                .model_stats
                .iter()
                .map(|r| {
                    (
                        r.key().clone(),
                        ModelStatsSnapshot {
                            prompt_tokens: r.value().prompt_tokens.load(Ordering::Relaxed),
                            completion_tokens: r.value().completion_tokens.load(Ordering::Relaxed),
                            requests: r.value().requests.load(Ordering::Relaxed),
                        },
                    )
                })
                .collect(),
            keys: self.key_stats_snapshot(),
        }
    }

    /// 生成 API Key 维度统计快照
    pub fn key_stats_snapshot(&self) -> HashMap<String, KeyUsageSnapshot> {
        self.key_stats
            .iter()
            .map(|r| {
                // 脱敏：只显示前 8 位
                let masked = mask_prefix(r.key(), 8);
                (
                    masked,
                    KeyUsageSnapshot {
                        prompt_tokens: r.value().prompt_tokens.load(Ordering::Relaxed),
                        completion_tokens: r.value().completion_tokens.load(Ordering::Relaxed),
                        requests: r.value().requests.load(Ordering::Relaxed),
                    },
                )
            })
            .collect()
    }
}

#[derive(Serialize)]
pub struct StatsSnapshot {
    pub total_requests: u64,
    pub success_requests: u64,
    pub failed_requests: u64,
    pub avg_latency_ms: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub uptime_secs: u64,
    pub models: HashMap<String, ModelStatsSnapshot>,
    pub keys: HashMap<String, KeyUsageSnapshot>,
}

#[derive(Serialize)]
pub struct ModelStatsSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: u64,
}

#[derive(Serialize)]
pub struct KeyUsageSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: u64,
}

/// 请求计时守卫，Drop 时自动记录统计
/// 如果未调用 mark_success/mark_failure，Drop 默认记录为失败
pub struct RequestTimer {
    stats: Arc<Stats>,
    start: Instant,
    marked: bool,
}

impl RequestTimer {
    pub fn new(stats: &Arc<Stats>) -> Self {
        Self {
            stats: Arc::clone(stats),
            start: Instant::now(),
            marked: false,
        }
    }
}

impl Drop for RequestTimer {
    fn drop(&mut self) {
        if !self.marked {
            let elapsed = self.start.elapsed();
            let latency = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
            self.stats.record_request(false, latency);
        }
    }
}

impl RequestTimer {
    /// 标记请求成功并记录统计
    pub fn mark_success(mut self) {
        let elapsed = self.start.elapsed();
        let latency = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
        self.stats.record_request(true, latency);
        self.marked = true;
    }

    /// 标记请求失败并记录统计
    pub fn mark_failure(mut self) {
        let elapsed = self.start.elapsed();
        let latency = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
        self.stats.record_request(false, latency);
        self.marked = true;
    }
}

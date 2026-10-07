//! 请求幂等性 —— `Idempotency-Key` 语义
//!
//! 客户端（SDK / 网关）在超时或网络抖动后重试同一个 `POST` 时，
//! 没有幂等保护就会**重复打到上游**：多消耗一次账号配额，也更容易触发风控。
//! 本模块按 Stripe / OpenAI 的既有约定实现：
//!
//! - 键的作用域是 `(API key, 方法+路径, Idempotency-Key)`；
//! - 同一个键 + **不同请求体** → `400 idempotency_error`（避免键复用导致串答案）；
//! - 同一个键 + 相同请求体：
//!   - 首次 → 正常执行，执行期间登记为 `InFlight`；
//!   - 执行中再次到达 → `409 idempotency_error`（并发重复不会再去打上游）；
//!   - 已完成 → 回放缓存的响应（含状态码 / `Content-Type` / 响应体），
//!     并带上 `idempotent-replayed: true`；
//! - 流式响应同样会被记录（逐块追加），因此流式请求也可回放；
//!   超过单条上限、或客户端中途断开导致记录不完整时，条目标记为不可回放，
//!   重试会得到 `409`，而不是一个被截断的假答案。
//!
//! 存储是进程内的有界 + TTL 缓存：重启即失效（与 `previous_response_id` 一致），
//! 绝不落盘（响应体可能包含用户内容）。

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::Stream;
use pin_project_lite::pin_project;

/// 幂等条目存活时间
const TTL: Duration = Duration::from_hours(24);
/// 条目数上限（超出后淘汰最旧的已完成条目）
const CAPACITY: usize = 1024;
/// 单条响应体记录上限（超出即标记为不可回放）
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// 所有条目响应体占用的总内存上限
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 已经记录完成的响应
#[derive(Debug, Clone)]
pub(crate) struct RecordedResponse {
    pub(crate) status: u16,
    pub(crate) content_type: &'static str,
    pub(crate) body: Bytes,
}

#[derive(Debug)]
enum EntryState {
    /// 首次请求正在执行
    InFlight,
    /// 已记录完整响应，可回放
    Completed(RecordedResponse),
    /// 响应体超限或记录中断，无法安全回放
    Unreplayable,
}

struct Entry {
    /// 请求指纹（方法 + 路径 + 请求体 + API key）
    fingerprint: u64,
    created: Instant,
    state: Mutex<EntryState>,
}

impl Entry {
    fn recorded_bytes(&self) -> usize {
        self.state.lock().map_or(0, |state| match &*state {
            EntryState::Completed(r) => r.body.len(),
            _ => 0,
        })
    }
}

struct Inner {
    entries: HashMap<String, Arc<Entry>>,
    /// 插入顺序，用于容量淘汰
    order: VecDeque<String>,
    recorded_bytes: usize,
}

/// 幂等判定结果
pub(crate) enum Begin {
    /// 首次执行；`guard` 负责在完成/中断时更新条目
    Fresh(IdempotencyGuard),
    /// 命中已完成记录，直接回放
    Replay(RecordedResponse),
    /// 同键请求正在执行
    InProgress,
    /// 同键但请求体不同
    Conflict,
    /// 有同键记录但内容不可回放（超限 / 上次中断）
    Unreplayable,
}

/// `Idempotency-Key` 缓存
pub(crate) struct IdempotencyStore {
    inner: Mutex<Inner>,
}

impl IdempotencyStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                recorded_bytes: 0,
            }),
        }
    }

    /// 尝试占位；`Begin::Fresh` 表示调用方获得执行权。
    ///
    /// `scope` 是 `(API key, 路径)`；`fingerprint` 用于识别「同键不同体」。
    /// 错误信封的形态由调用方（handler）决定，因此这里只返回判定结果。
    pub(crate) fn begin(self: &Arc<Self>, scope: &str, key: &str, fingerprint: u64) -> Begin {
        let cache_key = format!("{scope}\u{1}{key}");

        // 查找与占位必须在**同一次持锁**内完成：否则两个并发的同键请求
        // 都会看到「不存在」并各自打到上游，幂等保护就失效了。
        let entry = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.evict_expired();
            if let Some(existing) = inner.entries.get(&cache_key).cloned() {
                existing
            } else {
                let entry = Arc::new(Entry {
                    fingerprint,
                    created: Instant::now(),
                    state: Mutex::new(EntryState::InFlight),
                });
                inner.entries.insert(cache_key.clone(), entry.clone());
                inner.order.push_back(cache_key.clone());
                inner.evict_overflow();
                drop(inner);
                // 占位成功即首次执行；这里直接返回，避免再读一次状态
                return Begin::Fresh(IdempotencyGuard {
                    store: Arc::clone(self),
                    cache_key,
                    entry,
                    finished: false,
                });
            }
        };

        // 已有条目：在缓存锁之外读取状态（锁顺序固定为 map → entry）
        if entry.fingerprint != fingerprint {
            return Begin::Conflict;
        }
        let state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            EntryState::InFlight => Begin::InProgress,
            EntryState::Completed(r) => Begin::Replay(r.clone()),
            EntryState::Unreplayable => Begin::Unreplayable,
        }
    }

    fn commit(&self, cache_key: &str, entry: &Arc<Entry>, recorded: RecordedResponse) {
        let size = recorded.body.len();
        {
            let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
            *state = EntryState::Completed(recorded);
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.recorded_bytes += size;
        inner.enforce_memory_budget(cache_key);
    }

    fn mark_unreplayable(&self, cache_key: &str, entry: &Arc<Entry>) {
        {
            let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
            *state = EntryState::Unreplayable;
        }
        // 不可回放的条目没有保留价值（重试也不会回放），直接移除
        self.remove(cache_key, entry);
    }

    fn remove(&self, cache_key: &str, entry: &Arc<Entry>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner
            .entries
            .get(cache_key)
            .is_some_and(|e| Arc::ptr_eq(e, entry))
        {
            inner.entries.remove(cache_key);
            inner.order.retain(|k| k != cache_key);
            inner.recorded_bytes = inner.recorded_bytes.saturating_sub(entry.recorded_bytes());
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }
}

impl Inner {
    fn evict_expired(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.created) > TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some(entry) = self.entries.remove(&key) {
                self.recorded_bytes = self.recorded_bytes.saturating_sub(entry.recorded_bytes());
            }
            self.order.retain(|k| k != &key);
        }
    }

    fn evict_overflow(&mut self) {
        while self.entries.len() > CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.recorded_bytes = self.recorded_bytes.saturating_sub(entry.recorded_bytes());
            }
        }
    }

    /// 记录体量超过总预算时，从最旧的**已完成**条目开始淘汰
    fn enforce_memory_budget(&mut self, keep: &str) {
        while self.recorded_bytes > MAX_TOTAL_BYTES {
            let candidate = self
                .order
                .iter()
                .find(|k| {
                    k.as_str() != keep && self.entries.get(*k).is_some_and(|e| e.is_completed())
                })
                .cloned();
            let Some(key) = candidate else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.recorded_bytes = self.recorded_bytes.saturating_sub(entry.recorded_bytes());
            }
            self.order.retain(|k| k != &key);
        }
    }
}

impl Entry {
    fn is_completed(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|s| matches!(&*s, EntryState::Completed(_)))
    }
}

/// 首次执行的守卫：完成时落库，未完成即释放（含 panic / 提前返回）时撤销占位，
/// 让客户端的重试可以真正重跑，而不是撞上一个永远 `InFlight` 的键。
pub(crate) struct IdempotencyGuard {
    store: Arc<IdempotencyStore>,
    cache_key: String,
    entry: Arc<Entry>,
    finished: bool,
}

impl IdempotencyGuard {
    /// 记录一个已完成的整体响应（非流式 / 已聚合）
    pub(crate) fn complete(mut self, status: u16, content_type: &'static str, body: Bytes) {
        self.finished = true;
        self.store.commit(
            &self.cache_key,
            &self.entry,
            RecordedResponse {
                status,
                content_type,
                body,
            },
        );
    }

    /// 交给流式记录器：流正常结束时落库，中断时撤销占位
    pub(crate) fn into_stream_recorder(
        mut self,
        status: u16,
        content_type: &'static str,
    ) -> StreamRecorder {
        // 责任移交给记录器：此后本守卫的 Drop 不再撤销占位
        self.finished = true;
        StreamRecorder {
            store: Arc::clone(&self.store),
            cache_key: self.cache_key.clone(),
            entry: Arc::clone(&self.entry),
            status,
            content_type,
            buf: Vec::new(),
            overflowed: false,
            finished: false,
        }
    }
}

impl Drop for IdempotencyGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.store.remove(&self.cache_key, &self.entry);
        }
    }
}

/// 流式响应记录器：边发边记，仅在流正常结束时落库
pub(crate) struct StreamRecorder {
    store: Arc<IdempotencyStore>,
    cache_key: String,
    entry: Arc<Entry>,
    status: u16,
    content_type: &'static str,
    buf: Vec<u8>,
    overflowed: bool,
    finished: bool,
}

impl StreamRecorder {
    fn record(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.buf.len() + chunk.len() > MAX_BODY_BYTES {
            self.overflowed = true;
            self.buf = Vec::new();
            return;
        }
        self.buf.extend_from_slice(chunk);
    }

    fn finish(mut self) {
        self.finished = true;
        if self.overflowed {
            self.store.mark_unreplayable(&self.cache_key, &self.entry);
            return;
        }
        self.store.commit(
            &self.cache_key,
            &self.entry,
            RecordedResponse {
                status: self.status,
                content_type: self.content_type,
                body: Bytes::from(std::mem::take(&mut self.buf)),
            },
        );
    }

    fn abort(&mut self) {
        if !self.finished {
            self.finished = true;
            self.store.remove(&self.cache_key, &self.entry);
        }
    }
}

pin_project! {
    /// 记录 SSE 字节流，用于幂等回放
    pub(crate) struct RecordingStream<S> {
        #[pin]
        inner: S,
        recorder: Option<StreamRecorder>,
    }

    impl<S> PinnedDrop for RecordingStream<S> {
        fn drop(this: Pin<&mut Self>) {
            if let Some(recorder) = this.project().recorder.as_mut() {
                recorder.abort();
            }
        }
    }
}

impl<S> RecordingStream<S> {
    pub(crate) const fn new(inner: S, recorder: StreamRecorder) -> Self {
        Self {
            inner,
            recorder: Some(recorder),
        }
    }
}

impl<S, E> Stream for RecordingStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.inner.poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if let Some(recorder) = this.recorder.as_mut() {
                    recorder.record(&bytes);
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                // 客户端会看到一个错误流；这里不缓存半截结果
                if let Some(recorder) = this.recorder.as_mut() {
                    recorder.abort();
                }
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                if let Some(recorder) = this.recorder.take() {
                    recorder.finish();
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 由请求方法 + 路径 + 请求体 + API key 计算指纹
///
/// 只用进程内 `DefaultHasher`（SipHash，进程随机种子）：这些值不跨进程比较，
/// 也不落盘，无需稳定哈希。
pub(crate) fn fingerprint(scope: &str, body: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scope.hash(&mut hasher);
    body.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<IdempotencyStore> {
        Arc::new(IdempotencyStore::new())
    }

    #[test]
    fn fresh_then_replay() {
        let s = store();
        let fp = fingerprint("k:/v1/chat/completions", b"{\"a\":1}");
        let Begin::Fresh(guard) = s.begin("k:/v1/chat/completions", "idem-1", fp) else {
            panic!("首次请求应为 Fresh");
        };
        guard.complete(200, "application/json", Bytes::from_static(b"{}"));

        match s.begin("k:/v1/chat/completions", "idem-1", fp) {
            Begin::Replay(r) => {
                assert_eq!(r.status, 200);
                assert_eq!(r.body, Bytes::from_static(b"{}"));
            }
            _ => panic!("相同请求体应回放"),
        }
    }

    #[test]
    fn same_key_different_body_conflicts() {
        let s = store();
        let scope = "k:/v1/responses";
        let Begin::Fresh(_guard) = s.begin(scope, "idem-2", fingerprint(scope, b"a")) else {
            panic!("首次请求应为 Fresh");
        };
        assert!(matches!(
            s.begin(scope, "idem-2", fingerprint(scope, b"b")),
            Begin::Conflict
        ));
    }

    #[test]
    fn concurrent_duplicate_is_rejected() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"same");
        let Begin::Fresh(_guard) = s.begin(scope, "idem-3", fp) else {
            panic!("首次请求应为 Fresh");
        };
        assert!(matches!(s.begin(scope, "idem-3", fp), Begin::InProgress));
    }

    /// 未完成的守卫被丢弃（panic / 提前 return）后，键必须可以重新使用
    #[test]
    fn dropped_guard_frees_the_key() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"x");
        {
            let Begin::Fresh(_guard) = s.begin(scope, "idem-4", fp) else {
                panic!("首次请求应为 Fresh");
            };
        }
        assert_eq!(s.len(), 0, "未完成的占位应被撤销");
        assert!(matches!(s.begin(scope, "idem-4", fp), Begin::Fresh(_)));
    }

    /// 过期条目不再回放
    #[test]
    fn expired_entry_is_not_replayed() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"y");
        let Begin::Fresh(_guard) = s.begin(scope, "idem-5", fp) else {
            panic!("首次请求应为 Fresh");
        };
        {
            let inner = s.inner.lock().unwrap();
            let entry = inner
                .entries
                .get("k:/v1/chat/completions\u{1}idem-5")
                .expect("条目应存在");
            let expired = Arc::new(Entry {
                fingerprint: entry.fingerprint,
                created: Instant::now()
                    .checked_sub(TTL + Duration::from_secs(1))
                    .expect("单调时钟不会回退"),
                state: Mutex::new(EntryState::Completed(RecordedResponse {
                    status: 200,
                    content_type: "application/json",
                    body: Bytes::from_static(b"{}"),
                })),
            });
            drop(inner);
            let mut inner = s.inner.lock().unwrap();
            inner
                .entries
                .insert("k:/v1/chat/completions\u{1}idem-5".to_string(), expired);
        }
        assert!(matches!(s.begin(scope, "idem-5", fp), Begin::Fresh(_)));
    }

    /// 超过单条上限的流式记录不可回放，且条目被清理
    #[test]
    fn oversized_stream_is_not_replayable() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"z");
        let Begin::Fresh(guard) = s.begin(scope, "idem-6", fp) else {
            panic!("首次请求应为 Fresh");
        };
        let mut recorder = guard.into_stream_recorder(200, "text/event-stream");
        recorder.record(&vec![b'x'; MAX_BODY_BYTES + 1]);
        recorder.finish();
        assert_eq!(s.len(), 0, "不可回放条目应被移除");
    }

    /// 流式记录完成后可原样回放
    #[test]
    fn stream_recorder_replays_identical_bytes() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"w");
        let Begin::Fresh(guard) = s.begin(scope, "idem-7", fp) else {
            panic!("首次请求应为 Fresh");
        };
        let mut recorder = guard.into_stream_recorder(200, "text/event-stream");
        recorder.record(b"data: {\"a\":1}\n\n");
        recorder.record(b"data: [DONE]\n\n");
        recorder.finish();

        match s.begin(scope, "idem-7", fp) {
            Begin::Replay(r) => {
                assert_eq!(r.content_type, "text/event-stream");
                assert_eq!(&r.body[..], b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
            }
            _ => panic!("流式记录应可回放"),
        }
    }

    /// 流被完整消费 → 记录落库并可回放
    #[tokio::test]
    async fn recording_stream_commits_on_eof() {
        use futures::StreamExt as _;

        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"eof");
        let Begin::Fresh(guard) = s.begin(scope, "idem-8", fp) else {
            panic!("首次请求应为 Fresh");
        };
        let inner = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: 1\n\n")),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ]);
        let recorder = guard.into_stream_recorder(200, "text/event-stream");
        let mut stream = RecordingStream::new(inner, recorder);
        while stream.next().await.is_some() {}
        drop(stream);

        match s.begin(scope, "idem-8", fp) {
            Begin::Replay(r) => assert_eq!(&r.body[..], b"data: 1\n\ndata: [DONE]\n\n"),
            _ => panic!("完整消费的流应可回放"),
        }
    }

    /// 客户端中途断开（流被 drop）→ 撤销占位，重试可以真正重跑
    #[tokio::test]
    async fn recording_stream_aborts_on_drop() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        let fp = fingerprint(scope, b"drop");
        let Begin::Fresh(guard) = s.begin(scope, "idem-9", fp) else {
            panic!("首次请求应为 Fresh");
        };
        let inner = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from_static(
            b"data: 1\n\n",
        ))]);
        let recorder = guard.into_stream_recorder(200, "text/event-stream");
        let stream = RecordingStream::new(inner, recorder);
        drop(stream);

        assert_eq!(s.len(), 0, "中断的流不应留下可回放记录");
        assert!(matches!(s.begin(scope, "idem-9", fp), Begin::Fresh(_)));
    }

    /// 容量上限：最旧的已完成条目被淘汰，最新的仍在
    #[test]
    fn capacity_evicts_oldest() {
        let s = store();
        let scope = "k:/v1/chat/completions";
        for i in 0..CAPACITY + 5 {
            let key = format!("idem-{i}");
            let fp = fingerprint(scope, key.as_bytes());
            let Begin::Fresh(guard) = s.begin(scope, &key, fp) else {
                panic!("首次请求应为 Fresh");
            };
            guard.complete(200, "application/json", Bytes::from_static(b"{}"));
        }
        assert!(s.len() <= CAPACITY);
        let last = format!("idem-{}", CAPACITY + 4);
        let fp = fingerprint(scope, last.as_bytes());
        assert!(matches!(s.begin(scope, &last, fp), Begin::Replay(_)));
    }
}

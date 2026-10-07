//! 响应状态存储 —— 支撑 `previous_response_id` 的多轮上下文
//!
//! 设计取舍：本代理是**无状态**网关，无法像 OpenAI 一样长期保存响应。
//! 但 `previous_response_id` 是 Responses API 多轮对话的主路径
//! （Codex CLI / OpenAI Agents SDK 默认使用），完全不支持会让客户端
//! 每轮都丢失上下文。因此这里实现一个**进程内、有界、带 TTL** 的缓存：
//!
//! - 仅在 `store != Some(false)` 时写入（与 OpenAI 的 `store` 语义对齐）
//! - 容量上限 + 惰性淘汰，避免长时间运行后内存无界增长
//! - TTL 过期后 `previous_response_id` 变成未知 ID，由调用方返回 400
//!
//! 缓存只保存重新构造上下文所需的最小信息，不保存原始请求体。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 单条已保存的响应
#[derive(Debug, Clone)]
pub struct StoredTurn {
    /// 上一轮的实际用户输入（用于重建 user 消息）
    pub input_text: Option<String>,
    /// 模型产出的 output 数组（原样保存，重建时按类型解析）
    pub output: Vec<serde_json::Value>,
    /// 完整的 Response 对象快照（供 `GET /v1/responses/{id}` 原样返回）
    pub response: serde_json::Value,
}

/// 有界 + TTL 的响应缓存
///
/// 内部以 `Arc` 共享，`clone()` 只是增加引用计数 —— 流式响应的收尾钩子
/// 需要在流内部持有同一份缓存。
#[derive(Clone)]
pub struct ResponseStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    map: Mutex<HashMap<String, Entry>>,
    order: Mutex<std::collections::VecDeque<String>>,
    capacity: usize,
    ttl: Duration,
}

struct Entry {
    turn: StoredTurn,
    inserted: Instant,
}

impl ResponseStore {
    /// 创建缓存
    ///
    /// - `capacity`：最大保存条数（下限 1）
    /// - `ttl_secs`：条目存活秒数（下限 1）
    #[must_use]
    pub fn new(capacity: usize, ttl_secs: u64) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                map: Mutex::new(HashMap::new()),
                order: Mutex::new(std::collections::VecDeque::new()),
                capacity: capacity.max(1),
                ttl: Duration::from_secs(ttl_secs.max(1)),
            }),
        }
    }

    /// 保存一轮响应；超出容量时淘汰最旧条目
    pub fn insert(&self, id: String, turn: StoredTurn) {
        let Ok(mut map) = self.inner.map.lock() else {
            log::warn!(target: "responses_adapter", "failed to lock response cache, skipping store");
            return;
        };
        let Ok(mut order) = self.inner.order.lock() else {
            log::warn!(target: "responses_adapter", "failed to lock response cache, skipping store");
            return;
        };

        if map.contains_key(&id) {
            order.retain(|k| k != &id);
        }
        map.insert(
            id.clone(),
            Entry {
                turn,
                inserted: Instant::now(),
            },
        );
        order.push_back(id);

        while order.len() > self.inner.capacity {
            if let Some(oldest) = order.pop_front() {
                map.remove(&oldest);
            }
        }
    }

    /// 读取一轮响应；过期条目视为不存在并顺手清理
    #[must_use]
    pub fn get(&self, id: &str) -> Option<StoredTurn> {
        let Ok(mut map) = self.inner.map.lock() else {
            return None;
        };

        match map.get(id) {
            Some(entry) if entry.inserted.elapsed() <= self.inner.ttl => Some(entry.turn.clone()),
            Some(_) => {
                map.remove(id);
                drop(map);
                if let Ok(mut order) = self.inner.order.lock() {
                    order.retain(|k| k != id);
                }
                None
            }
            None => None,
        }
    }

    /// 取出用于 `GET /v1/responses/{id}` 的完整响应对象快照
    ///
    /// 与 `get` 同样受容量与 TTL 约束：过期 / 被淘汰即返回 `None`（调用方回 404）。
    #[must_use]
    pub fn get_response(&self, id: &str) -> Option<serde_json::Value> {
        self.get(id).map(|turn| turn.response)
    }

    /// 当前保存条数（测试用）
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.map.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// 缓存是否为空（测试用）
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ResponseStore {
    fn default() -> Self {
        Self::new(256, 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str) -> StoredTurn {
        StoredTurn {
            input_text: Some(text.to_string()),
            output: vec![serde_json::json!({"type": "message", "content": []})],
            response: serde_json::json!({
                "id": "resp_test",
                "object": "response",
                "status": "completed",
                "output": [{"type": "message", "content": []}],
            }),
        }
    }

    #[test]
    fn get_response_returns_snapshot_and_misses_after_eviction() {
        let store = ResponseStore::new(1, 60);
        store.insert("resp_1".to_string(), turn("hi"));
        let snap = store.get_response("resp_1").expect("snapshot");
        assert_eq!(snap["object"], "response");
        assert_eq!(snap["status"], "completed");
        assert!(store.get_response("nope").is_none());

        // 容量为 1：插入新条目后旧快照被淘汰 → 404 语义
        store.insert("resp_2".to_string(), turn("yo"));
        assert!(store.get_response("resp_1").is_none());
        assert!(store.get_response("resp_2").is_some());
    }

    #[test]
    fn insert_and_get() {
        let store = ResponseStore::new(4, 60);
        store.insert("resp_1".to_string(), turn("hi"));
        let got = store.get("resp_1").unwrap();
        assert_eq!(got.input_text.as_deref(), Some("hi"));
        assert!(store.get("resp_missing").is_none());
    }

    #[test]
    fn capacity_evicts_oldest() {
        let store = ResponseStore::new(2, 60);
        store.insert("a".to_string(), turn("1"));
        store.insert("b".to_string(), turn("2"));
        store.insert("c".to_string(), turn("3"));
        assert!(store.get("a").is_none(), "oldest entry must be evicted");
        assert!(store.get("b").is_some());
        assert!(store.get("c").is_some());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn ttl_expiry_removes_entry() {
        let store = ResponseStore::new(4, 1);
        store.insert("expiring".to_string(), turn("x"));
        // 直接把 inserted 时间拨回过去
        {
            let mut map = store.inner.map.lock().unwrap();
            if let Some(e) = map.get_mut("expiring") {
                e.inserted = Instant::now() - Duration::from_secs(10);
            }
        }
        assert!(store.get("expiring").is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn clones_share_state() {
        let store = ResponseStore::new(4, 60);
        let clone = store.clone();
        clone.insert("resp_x".to_string(), turn("shared"));
        assert!(store.get("resp_x").is_some());
    }
}

//! 对话模块 —— 请求分流与响应流处理
//!
//! 通过 accounts 模块获取账号资源，将 prompt 按大小分发到不同的请求路径，
//! 返回带账号守卫的 SSE 字节流。

mod request;
mod response;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::join_all;

use crate::accounts::{Accounts, StopStreamPayload};
use crate::config::DsCoreConfig;
use response::ActiveSession;

pub use request::{ChatRequest, ChatResponse, DeltaPrompt, FilePayload};
pub use response::StreamEvent;

/// 一次请求的会话决策
struct SessionPlan {
    session_id: String,
    /// 是否为**增量复用**（true = 沿用缓存会话，只发新增消息）
    reuse: bool,
    /// 增量发送时的 `parent_message_id`（上游上一条响应的消息 id）
    parent_message_id: Option<i64>,
}

/// 对话模块的统一入口
///
/// 持有对 accounts 的引用，负责 prompt 分流并返回包装后的流。
pub struct Chat {
    accounts: Arc<Accounts>,
    active_sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
    model_types: Vec<String>,
    input_character_limits: Vec<u32>,
    /// 是否复用账号会话（真实客户端一个会话长期复用；默认关闭 = 每轮建删）
    session_reuse: bool,
}

impl Chat {
    /// 创建对话模块
    pub fn new(accounts: Arc<Accounts>, config: &DsCoreConfig) -> Self {
        Self {
            accounts,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            model_types: config.model_types.clone(),
            input_character_limits: config.input_character_limits.clone(),
            session_reuse: config.session_policy == "reuse",
        }
    }

    /// 本次请求的会话决策
    ///
    /// `reuse = true` 表示沿用账号上缓存的会话做**增量发送**：
    /// 只把新增的那条用户消息发上去（`parent_message_id` 指向上一轮响应），
    /// 由上游自己组装上下文 —— 与官方客户端一致（见 `docs/development.md`）。
    async fn acquire_session(
        &self,
        account: &crate::accounts::Account,
        delta: Option<&request::DeltaPrompt>,
    ) -> Result<SessionPlan, crate::CoreError> {
        if self.session_reuse
            && let Some(delta) = delta
            && let Some((session_id, parent)) = account.cached_session_for(&delta.chain)
        {
            log::debug!(
                target: "ds_core::accounts",
                "增量复用会话: account={}, session={session_id}, parent={parent}, known={}",
                account.display_id(),
                delta.chain.len()
            );
            return Ok(SessionPlan {
                session_id,
                reuse: true,
                parent_message_id: Some(parent),
            });
        }
        let session_id = self.accounts.create_session(account).await?;
        Ok(SessionPlan {
            session_id,
            reuse: false,
            parent_message_id: None,
        })
    }

    /// 获取指定 model_type 的 input_character_limit
    fn input_character_limit_for(&self, model_type: &str) -> usize {
        self.model_types
            .iter()
            .position(|t| t == model_type)
            .and_then(|i| self.input_character_limits.get(i))
            .copied()
            .map_or(163_840, |v| v as usize)
    }

    /// 优雅关闭：清理所有残留的活跃 session
    pub async fn shutdown(&self) {
        let sessions = {
            let mut map = self.active_sessions.lock().unwrap();
            std::mem::take(&mut *map)
        };

        if sessions.is_empty() {
            return;
        }

        log::info!(
            target: "ds_core::accounts",
            "shutdown: 清理 {} 个残留 session", sessions.len()
        );

        let futures: Vec<_> = sessions
            .into_values()
            .map(|s| async move {
                let payload = StopStreamPayload {
                    chat_session_id: s.session_id.clone(),
                    message_id: s.message_id,
                };
                let client = s.client.clone();
                if let Err(e) = client.stop_stream(&s.token, &payload).await {
                    log::warn!(
                        target: "ds_core::accounts",
                        "shutdown 停止 session {} 失败: {}",
                        s.session_id, e
                    );
                }
                if let Err(e) = client.delete_session(&s.token, &s.session_id).await {
                    log::warn!(
                        target: "ds_core::accounts",
                        "shutdown 清理 session {} 失败: {}",
                        s.session_id, e
                    );
                }
            })
            .collect();
        join_all(futures).await;
    }
}

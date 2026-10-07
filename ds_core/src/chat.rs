//! 对话模块 —— 请求分流与响应流处理
//!
//! 通过 accounts 模块获取账号资源，将 prompt 按大小分发到不同的请求路径，
//! 返回带账号守卫的 SSE 字节流。

mod request;
mod response;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::accounts::Accounts;
use crate::config::DsCoreConfig;
use response::ActiveSession;

pub use request::{ChatRequest, ChatResponse, FilePayload};
pub use response::StreamEvent;

/// 对话模块的统一入口
///
/// 持有对 accounts 的引用，负责 prompt 分流并返回包装后的流。
pub struct Chat {
    accounts: Arc<Accounts>,
    active_sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
    model_types: Vec<String>,
    input_character_limits: Vec<u32>,
}

impl Chat {
    /// 创建对话模块
    pub fn new(accounts: Arc<Accounts>, config: &DsCoreConfig) -> Self {
        Self {
            accounts,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            model_types: config.model_types.clone(),
            input_character_limits: config.input_character_limits.clone(),
        }
    }

    /// 获取指定 model_type 的 input_character_limit
    fn input_character_limit_for(&self, model_type: &str) -> usize {
        self.model_types
            .iter()
            .position(|t| t == model_type)
            .and_then(|i| self.input_character_limits.get(i))
            .copied()
            .map(|v| v as usize)
            .unwrap_or(163_840)
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

        use crate::accounts::StopStreamPayload;
        use futures::future::join_all;

        let futures: Vec<_> = sessions
            .into_values()
            .map(|s| async move {
                let payload = StopStreamPayload {
                    chat_session_id: s.session_id.clone(),
                    message_id: s.message_id,
                };
                let client = s.client.clone();
                let _ = client.stop_stream(&s.token, &payload).await;
                let _ = client
                    .delete_session(&s.token, &s.session_id)
                    .await
                    .inspect_err(|e| {
                        log::warn!(
                            target: "ds_core::accounts",
                            "shutdown 清理 session {} 失败: {}",
                            s.session_id, e
                        );
                    });
            })
            .collect();
        join_all(futures).await;
    }
}

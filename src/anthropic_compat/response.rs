//! Anthropic 响应映射 —— 将 OpenAI ChatCompletion 映射为 Anthropic Message
//!
//! 门面模块：声明子模块，暴露共享类型和辅助函数。
//! `MessagesResponse` / `Usage` 定义在 `types.rs`（与请求类型同模块）。

mod aggregate;
mod stream;

pub(crate) use aggregate::from_chat_completions;
pub(crate) use stream::from_chat_completion_stream;

/// 响应内容块——在 `types.rs` 中定义为 `ResponseContentBlock`，此处别名保持子模块兼容
pub(crate) use crate::anthropic_compat::types::ResponseContentBlock as ContentBlock;

// ============================================================================
// 共享辅助函数
// ============================================================================

/// OpenAI `finish_reason` → Anthropic `stop_reason`
///
/// Anthropic 的 `StopReason` 枚举只有
/// `end_turn | max_tokens | stop_sequence | tool_use | pause_turn | refusal |
/// model_context_window_exceeded`。直接把 OpenAI 的值透传会产生客户端无法识别的
/// 取值（例如 `length` / `content_filter`），必须显式映射。
pub(crate) fn finish_reason_map(reason: &str) -> String {
    match reason {
        "stop" | "end_turn" => "end_turn".to_string(),
        "tool_calls" | "function_call" | "tool_use" => "tool_use".to_string(),
        "length" | "max_tokens" => "max_tokens".to_string(),
        "content_filter" | "refusal" => "refusal".to_string(),
        // 未知取值退化为自然结束，避免发出非法枚举值
        other => {
            log::warn!(
                target: "anthropic_compat::response",
                "未知 finish_reason {other:?}，按 end_turn 处理"
            );
            "end_turn".to_string()
        }
    }
}

/// OpenAI id 格式为 chatcmpl-xxx，映射为 msg_xxx
pub(crate) fn map_id(openai_id: &str) -> String {
    openai_id
        .strip_prefix("chatcmpl-")
        .map(|hex| format!("msg_{hex}"))
        .or_else(|| {
            openai_id
                .strip_prefix("call_")
                .map(|suffix| format!("toolu_{suffix}"))
        })
        .unwrap_or_else(|| format!("msg_{openai_id}"))
}

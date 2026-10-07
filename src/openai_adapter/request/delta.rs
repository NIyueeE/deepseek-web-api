//! 增量 prompt 构造 —— 让「复用会话」的请求形态对齐官方客户端
//!
//! 官方 Web 客户端把**最新一条用户消息**放进 `/chat/completion` 的 `prompt`，
//! 历史保存在服务端会话里、靠 `parent_message_id` 串联（前端 bundle 中不存在任何
//! `<｜Role｜>` 之类的标签字面量）。本代理为了让每轮请求自成一体，历史上一直把整段
//! 历史渲染进 prompt；当会话可以复用时（`session_policy = reuse`），这里给出
//! 「只发新增内容」所需的两个东西：
//!
//! 1. `chain`：消息序列的累积指纹链，用于确认上游会话已知的内容与客户端发来的历史
//!    严格一致（前缀匹配，见 `Account::cached_session_for`）；
//! 2. `text`：新增那条用户消息的**原文**（不含任何角色标签）。
//!
//! **只在纯对话形态下提供**：一旦涉及工具定义 / 工具消息 / 文件 / 联网搜索 /
//! `response_format` 注入，prompt 里就必然带有代理特有的脚手架文本，增量发送会破
//! 坏这些能力，因此这些情况一律返回 `None`（调用方退回「新建会话 + 完整 prompt」，
//! 也就是改动前的行为）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::openai_adapter::types::{ChatCompletionsRequest, Message};
use ds_core::DeltaPrompt;

use super::prompt::format_content;

use super::tools::ToolContext;

/// 单条消息的指纹
///
/// 只用于「历史是否与上游会话一致」的判断：不要求密码学强度，但必须对
/// `role` + `content` 的任何变化敏感（不一致只会导致退回新建会话，是安全的一侧）。
fn message_fingerprint(msg: &Message) -> u64 {
    let mut hasher = DefaultHasher::new();
    msg.role.hash(&mut hasher);
    match msg.content.as_ref() {
        Some(content) => format_content(content).hash(&mut hasher),
        None => 0u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// 消息序列的累积指纹链（第 i 项 = 前 i+1 条消息的哈希）
fn chain_of(messages: &[Message]) -> Vec<u64> {
    let mut acc = DefaultHasher::new();
    let mut chain = Vec::with_capacity(messages.len());
    for msg in messages {
        message_fingerprint(msg).hash(&mut acc);
        chain.push(acc.finish());
    }
    chain
}

/// 构造增量 prompt；形态不匹配时返回 `None`（调用方退回完整 prompt）
pub(crate) fn build(
    req: &ChatCompletionsRequest,
    tool_ctx: &ToolContext,
    has_files: bool,
    has_http_urls: bool,
) -> Option<DeltaPrompt> {
    // 1) 工具 / 文件 / 注入类能力一律不参与增量（prompt 里有代理脚手架）
    if tool_ctx.defs_text.is_some()
        || tool_ctx.format_block.is_some()
        || tool_ctx.instruction_text.is_some()
        || has_files
        || has_http_urls
        || req.response_format.is_some()
    {
        return None;
    }

    // 2) 消息形态：非空、无 tool 角色、最后一条是纯文本 user
    let messages = req.messages.as_slice();
    let last = messages.last()?;
    if last.role != "user" || messages.iter().any(|m| m.role == "tool") {
        return None;
    }
    let content = last.content.as_ref()?;
    let text = format_content(content);

    Some(DeltaPrompt {
        chain: chain_of(messages),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_adapter::types::ChatCompletionsRequest;
    use serde_json::json;

    fn parse(body: serde_json::Value) -> ChatCompletionsRequest {
        serde_json::from_value(body).expect("parse request")
    }

    fn plain_chat() -> ChatCompletionsRequest {
        parse(json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "你好" },
                { "role": "assistant", "content": "你好！有什么可以帮你的？" },
                { "role": "user", "content": "帮我写一句诗" }
            ]
        }))
    }

    fn no_tool_ctx() -> ToolContext {
        ToolContext {
            defs_text: None,
            format_block: None,
            instruction_text: None,
        }
    }

    /// 纯对话：增量文本就是最后一条用户消息的原文，且不含任何角色标签
    #[test]
    fn plain_chat_yields_tag_free_delta() {
        let req = plain_chat();
        let delta = build(&req, &no_tool_ctx(), false, false).expect("应提供增量");
        assert_eq!(delta.text, "帮我写一句诗");
        assert_eq!(delta.chain.len(), 3, "链长度 = 消息数");
        for tag in ["<｜", "｜>", "tool▁", "<|"] {
            assert!(
                !delta.text.contains(tag),
                "增量文本不应含角色标签 {tag}: {}",
                delta.text
            );
        }
    }

    /// 同一条历史前缀 → 链前缀稳定（客户端原样回传历史时必须一致）
    #[test]
    fn chain_prefix_is_stable_across_turns() {
        let first = parse(json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "第一轮" }]
        }));
        let second = parse(json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "第一轮" },
                { "role": "assistant", "content": "回复" },
                { "role": "user", "content": "第二轮" }
            ]
        }));
        let d1 = build(&first, &no_tool_ctx(), false, false).expect("delta 1");
        let d2 = build(&second, &no_tool_ctx(), false, false).expect("delta 2");
        assert_eq!(d1.chain.len(), 1);
        assert_eq!(
            d2.chain[..1],
            d1.chain[..],
            "第二轮的前缀必须与第一轮完全一致"
        );
    }

    /// 历史被改写（同前缀内容变化）→ 链不再匹配，调用方会退回新建会话
    #[test]
    fn edited_history_changes_chain() {
        let orig = plain_chat();
        let mut edited = plain_chat();
        edited.messages[0].content = serde_json::from_value(json!("你好（被改写）")).ok();
        let a = build(&orig, &no_tool_ctx(), false, false).expect("delta a");
        let b = build(&edited, &no_tool_ctx(), false, false).expect("delta b");
        assert_ne!(a.chain[0], b.chain[0]);
    }

    /// 工具 / 文件 / 搜索 / response_format 一律不提供增量
    #[test]
    fn tool_capable_requests_have_no_delta() {
        let req = plain_chat();
        let with_tools = parse(json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{
                "type": "function",
                "function": { "name": "f", "parameters": { "type": "object" } }
            }]
        }));
        let tool_ctx =
            crate::openai_adapter::request::tools::extract(&with_tools).expect("extract tools");
        assert!(
            build(&with_tools, &tool_ctx, false, false).is_none(),
            "带工具的请求必须退回完整 prompt"
        );

        assert!(build(&req, &no_tool_ctx(), true, false).is_none(), "文件");
        assert!(build(&req, &no_tool_ctx(), false, true).is_none(), "搜索");

        let rf = parse(json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "json_object" }
        }));
        assert!(
            build(&rf, &no_tool_ctx(), false, false).is_none(),
            "response_format 注入"
        );
    }

    /// 最后一条不是 user（例如 tool 结果 / assistant 续写）→ 不提供增量
    #[test]
    fn non_user_tail_has_no_delta() {
        let req = parse(json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "tool", "content": "工具结果" }
            ]
        }));
        assert!(build(&req, &no_tool_ctx(), false, false).is_none());
    }
}

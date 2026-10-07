//! Prompt 构建 —— 将 OpenAI messages 转换为 DeepSeek 原生 ChatML 标签格式
//!
//! 角色标记使用 `<｜System｜>`、`<｜User｜>`、`<｜Assistant｜>`、`<｜tool▁outputs▁begin｜>`，
//! 上一轮以 `<｜end▁of▁sentence｜>` 收尾，与 DeepSeek 官方对话模板一致。
//!
//! 工具定义、调用格式规范与 `response_format` 约束作为**普通 System 内容注入一次**，
//! 不使用「未闭合 `<think>` + 元指令」的注入方式（原因见 `build()` 内注释）。

use super::tools::ToolContext;
use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{ChatCompletionsRequest, ContentPart, Message, MessageContent};

/// 合并连续相同 role 的 message，避免 DeepSeek 模型对连续同角色标签产生混淆
fn merge_messages(messages: &[Message]) -> Vec<Message> {
    let mut merged: Vec<Message> = Vec::new();
    for msg in messages {
        if let Some(last) = merged.last_mut()
            && last.role == msg.role
            && msg.role != "tool"
        // tool 由 build() 分组合并
        {
            // 合并 content
            if let Some(ref content) = msg.content {
                match &mut last.content {
                    Some(last_content) => match (last_content, content) {
                        (MessageContent::Text(a), MessageContent::Text(b)) => {
                            a.push('\n');
                            a.push_str(b);
                        }
                        (MessageContent::Parts(a), MessageContent::Parts(b)) => {
                            a.extend(b.clone());
                        }
                        // 不同类型 → 都转 text 拼接
                        (last_c, new_c) => {
                            let new_text = format_content(new_c);
                            let last_text = format_content(last_c);
                            *last_c = MessageContent::Text(format!("{last_text}\n{new_text}"));
                        }
                    },
                    None => {
                        last.content.clone_from(&msg.content);
                    }
                }
            }
            // 合并 tool_calls
            if let Some(ref calls) = msg.tool_calls {
                match &mut last.tool_calls {
                    Some(last_calls) => last_calls.extend(calls.clone()),
                    None => last.tool_calls.clone_from(&msg.tool_calls),
                }
            }
            // 覆盖字段：取最后一条的值
            if msg.name.is_some() {
                last.name.clone_from(&msg.name);
            }
            if msg.tool_call_id.is_some() {
                last.tool_call_id.clone_from(&msg.tool_call_id);
            }
            if msg.function_call.is_some() {
                last.function_call.clone_from(&msg.function_call);
            }
            if msg.refusal.is_some() {
                last.refusal.clone_from(&msg.refusal);
            }
            if msg.audio.is_some() {
                last.audio.clone_from(&msg.audio);
            }
            continue;
        }
        merged.push(msg.clone());
    }
    merged
}

/// 生成 response_format 对应的提示文本
fn format_response_text(rf: &crate::openai_adapter::types::ResponseFormat) -> String {
    match rf.ty.as_str() {
        "json_object" => {
            "请直接输出合法的 JSON 对象，不要包含任何 markdown 代码块标记或其他解释性文字。".into()
        }
        "json_schema" => {
            let schema_text = rf
                .json_schema
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_default())
                .unwrap_or_default();
            if schema_text.is_empty() {
                "以 JSON 的形式输出。".into()
            } else {
                format!(
                    "以 JSON 的形式输出，输出的 JSON 需遵守以下的格式：\n\n~~~json\n{schema_text}\n~~~"
                )
            }
        }
        "text" => String::new(),
        _ => format!("请以 {} 格式输出。", rf.ty),
    }
}

/// 构建 DeepSeek 原生标签格式的 prompt 字符串
///
/// 顺序：`<｜System｜>`（工具定义 / 格式规范 / 调用指令 / `response_format` 约束，
/// 合并为普通 System 内容**注入一次**）→ 历史 user/tool/assistant 轮次 →
/// 末尾补 `<｜Assistant｜>` 锚点（最后一条已是 assistant 则保持原样）。
/// 把连续的 tool 输出包成 `<｜tool▁outputs▁begin｜>…<｜tool▁outputs▁end｜>` 块
fn wrap_tool_outputs(outputs: &[String]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(outputs.iter().map(String::len).sum::<usize>() + 64);
    for c in outputs {
        let _ = write!(out, "<｜tool▁output▁begin｜>{c}<｜tool▁output▁end｜>");
    }
    out
}

pub(crate) fn build(req: &ChatCompletionsRequest, tool_ctx: &ToolContext) -> String {
    let messages = merge_messages(&req.messages);
    let mut parts: Vec<String> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "tool" {
            let mut tool_contents = Vec::new();
            while i < messages.len() && messages[i].role == "tool" {
                if let Some(c) = &messages[i].content {
                    tool_contents.push(format_content(c));
                }
                i += 1;
            }
            let inner = wrap_tool_outputs(&tool_contents);
            parts.push(format!(
                "<｜tool▁outputs▁begin｜>{inner}<｜tool▁outputs▁end｜>"
            ));
        } else {
            parts.push(format_message(&messages[i]));
            i += 1;
        }
    }

    // 工具定义、格式规范、调用指令与输出格式约束全部作为普通 System 内容注入一次。
    //
    // 不再使用「未闭合 <think> + 元指令」的注入方式：那种写法会把
    // 「嗯，我刚刚被系统提醒需要遵循以下内容」这类角色扮演文本和重复两遍的
    // 规则块送进模型，既抬高 token 成本（实测约 1.8 倍），也容易被上游
    // 滥用检测判定为提示词注入。标准 ChatML 下模型遵循度实测一致。
    let mut system_sections: Vec<String> = Vec::new();

    if let Some(text) = tool_ctx.defs_text.as_deref() {
        system_sections.push(text.to_string());
    }
    if let Some(text) = tool_ctx.format_block.as_deref() {
        system_sections.push(text.to_string());
    }
    if let Some(text) = tool_ctx.instruction_text.as_deref() {
        system_sections.push(text.to_string());
    }
    // response_format 降级：以普通文本形式描述输出格式约束
    if let Some(rf) = req.response_format.as_ref() {
        let format_text = format_response_text(rf);
        if !format_text.is_empty() {
            system_sections.push(format_text);
        }
    }

    if !system_sections.is_empty() {
        let body = system_sections.join("\n\n");
        if let Some(sys) = parts.iter_mut().find(|p| p.starts_with("<｜System｜>")) {
            // 已有 System：把工具信息插到该消息正文末尾（保持角色标签在最前）
            let insert_at = sys.rfind('\n').unwrap_or(sys.len());
            sys.insert_str(insert_at, &format!("\n\n{body}"));
        } else {
            parts.insert(0, format!("<｜System｜>{body}\n"));
        }
    }

    // 末尾必须有 <｜Assistant｜> 作为生成起点，同时供 split_history_prompt 定位拆分点。
    //
    // 这里必须判断「最后一个片段」而不是「是否出现过」：多轮历史里本来就含
    // <｜Assistant｜> 轮次，早期写法用 `any()` 会导致末尾缺少锚点，
    // split_history_prompt 找不到 assistant 块，整段历史被当作 inline prompt 直发。
    // 若最后一条消息本身就是 assistant（prefill 续写场景），则保持原样不再追加。
    if !parts
        .last()
        .is_some_and(|p| p.starts_with("<｜Assistant｜>"))
    {
        parts.push("<｜Assistant｜>\n".to_string());
    }

    parts.join("")
}

fn role_tag(role: &str) -> String {
    let mut r = role.to_string();
    if let Some(c) = r.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    format!("<｜{r}｜>")
}

fn format_message(msg: &Message) -> String {
    let body = match msg.role.as_str() {
        "assistant" => format_assistant(msg),
        "tool" => format_tool(msg),
        "function" => format_function(msg),
        _ => format_generic(msg),
    };
    let tag = if msg.role == "tool" {
        String::new() // tool 用自有标签，不需要 <｜Tool｜>
    } else {
        role_tag(&msg.role)
    };
    let prefix = if msg.role == "user" {
        "<｜end▁of▁sentence｜>"
    } else {
        ""
    };
    format!("{prefix}{tag}{body}")
}

fn format_generic(msg: &Message) -> String {
    let mut parts = Vec::new();
    if let Some(name) = &msg.name {
        parts.push(format!("(name: {name})"));
    }
    if let Some(content) = &msg.content {
        parts.push(format_content(content));
    }
    parts.join("\n")
}

fn format_assistant(msg: &Message) -> String {
    let mut parts = Vec::new();
    if let Some(content) = &msg.content {
        parts.push(format_content(content));
    }
    if let Some(tool_calls) = &msg.tool_calls {
        let items: Vec<String> = tool_calls
            .iter()
            .filter_map(|tc| {
                tc.function.as_ref().map(|func| {
                    let args = serde_json::from_str::<serde_json::Value>(&func.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    format!(
                        "{{\"name\": {}, \"arguments\": {}}}",
                        serde_json::to_string(&func.name).unwrap_or_else(|_| "\"\"".into()),
                        serde_json::to_string(&args).unwrap_or_else(|_| "null".into()),
                    )
                })
            })
            .collect();
        parts.push(format!(
            "{TOOL_CALL_START}\n[{}]\n{TOOL_CALL_END}",
            items.join(", ")
        ));
    }
    if let Some(fc) = &msg.function_call {
        let args = serde_json::from_str::<serde_json::Value>(&fc.arguments)
            .unwrap_or(serde_json::Value::Null);
        let item = format!(
            "{{\"name\": {}, \"arguments\": {}}}",
            serde_json::to_string(&fc.name).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(&args).unwrap_or_else(|_| "null".into()),
        );
        parts.push(format!("{TOOL_CALL_START}\n[{item}]\n{TOOL_CALL_END}"));
    }
    if let Some(refusal) = &msg.refusal {
        parts.push(format!("(refusal: {refusal})"));
    }
    parts.join("\n")
}

fn format_tool(msg: &Message) -> String {
    let content = msg.content.as_ref().map(format_content).unwrap_or_default();
    format!(
        "<｜tool▁outputs▁begin｜><｜tool▁output▁begin｜>{content}<｜tool▁output▁end｜><｜tool▁outputs▁end｜>"
    )
}

fn format_function(msg: &Message) -> String {
    let mut parts = Vec::new();
    if let Some(name) = &msg.name {
        parts.push(format!("(name: {name})"));
    }
    if let Some(content) = &msg.content {
        parts.push(format_content(content));
    }
    parts.join("\n")
}

pub(crate) fn format_content(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => {
            parts.iter().map(format_part).collect::<Vec<_>>().join("\n")
        }
    }
}

fn format_part(part: &ContentPart) -> String {
    match part.ty.as_str() {
        "text" => part.text.clone().unwrap_or_default(),
        "refusal" => part.refusal.clone().unwrap_or_default(),
        "image_url" => part.image_url.as_ref().map_or_else(
            || "[图片]".to_string(),
            |img| {
                if img.url.starts_with("http://") || img.url.starts_with("https://") {
                    format!("[请访问这个链接: {}]", img.url)
                } else {
                    let detail = img.detail.as_deref().unwrap_or("auto");
                    format!("[图片: detail={detail}]")
                }
            },
        ),
        "input_audio" => {
            let fmt = part
                .input_audio
                .as_ref()
                .map_or("unknown", |a| a.format.as_str());
            format!("[音频: format={fmt}]")
        }
        "file" => {
            let filename = part
                .file
                .as_ref()
                .and_then(|f| f.filename.as_deref())
                .unwrap_or("unknown");
            let desc = part.text.as_deref().filter(|t| !t.is_empty());
            desc.map_or_else(
                || format!("[文件: filename={filename}]"),
                |d| format!("[文件: {d} (filename={filename})]"),
            )
        }
        _ => format!("[未支持的内容类型: {}]", part.ty),
    }
}

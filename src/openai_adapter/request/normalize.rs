//! 请求校验与默认值收敛
//!
//! 职责：验证必填字段、消息格式，并将可选参数收敛为内部使用的标准化值。

use crate::openai_adapter::types::{ChatCompletionsRequest, StopSequence};

pub(crate) struct NormalizedParams {
    pub include_usage: bool,
    pub include_obfuscation: bool,
    pub stop: Vec<String>,
}

/// 收敛并返回标准化参数
///
/// 校验规则：
/// - model 不能为空
/// - messages 不能为空
/// - role=tool 的消息必须包含 tool_call_id
/// - role=function 的消息必须包含 name
pub(crate) fn apply(req: &ChatCompletionsRequest) -> Result<NormalizedParams, String> {
    if req.model.trim().is_empty() {
        return Err("缺少必填字段 'model'".into());
    }

    if req.messages.is_empty() {
        return Err("缺少必填字段 'messages'".into());
    }

    for (i, msg) in req.messages.iter().enumerate() {
        match msg.role.as_str() {
            "tool" if msg.tool_call_id.is_none() => {
                return Err(format!(
                    "messages[{i}] 角色为 'tool' 时必须提供 'tool_call_id'"
                ));
            }
            "function" if msg.name.is_none() => {
                return Err(format!("messages[{i}] 角色为 'function' 时必须提供 'name'"));
            }
            _ => {}
        }
    }

    let include_usage = req.stream_options.as_ref().is_some_and(|o| o.include_usage);

    let include_obfuscation = req
        .stream_options
        .as_ref()
        .is_none_or(|o| o.include_obfuscation);

    let stop = match &req.stop {
        Some(StopSequence::Single(s)) => vec![s.clone()],
        Some(StopSequence::Multiple(v)) => v.clone(),
        None => Vec::new(),
    };

    Ok(NormalizedParams {
        include_usage,
        include_obfuscation,
        stop,
    })
}

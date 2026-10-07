//! Responses API 请求映射 —— ResponsesRequest → ChatCompletionsRequest
//!
//! 纯函数：结构体到结构体的字段映射，不访问网络与全局状态。
//!
//! Responses API 与 Chat Completions 的核心差异（本模块负责抹平）：
//! - `input` 可为字符串或输入项数组；消息项的 `type` 字段可省略
//! - `instructions` 等价于 Chat Completions 的 system 消息
//! - 工具定义是扁平结构（`{type, name, parameters}`），不是 `{type, function:{...}}`
//! - `function_call` / `function_call_output` 是顶层输入项，对应 assistant.tool_calls
//!   与 role=tool 消息
//! - `text.format` 等价于 `response_format`
//! - `reasoning.effort` 等价于 `reasoning_effort`
//! - `max_output_tokens` 等价于 `max_tokens`

use crate::openai_adapter::types::{
    ChatCompletionsRequest, ContentPart, CustomTool, FileContent, FunctionCall, FunctionDefinition,
    ImageUrlContent, Message, MessageContent as OaiMessageContent, NamedCustom, NamedCustomChoice,
    NamedFunction, NamedToolChoice, ResponseFormat, StreamOptions, Tool, ToolCall, ToolChoice,
    WebSearchOptions,
};

use super::types::{
    Input, InputContent, InputContentPart, InputItem, Instructions, ReasoningParam,
    ResponsesRequest, ResponsesTool, TextFormat,
};

/// 将 Responses 请求映射为 ChatCompletionsRequest
///
/// `history` 为 `previous_response_id` 解析出的历史消息（不含 system）；
/// 其 system 指令不会沿用（与 OpenAI 语义一致：新请求的 `instructions` 覆盖）。
pub(crate) fn into_chat_completions(
    req: &ResponsesRequest,
    history: Vec<Message>,
) -> Result<ChatCompletionsRequest, String> {
    let mut messages: Vec<Message> = Vec::new();

    // instructions → 前置 system 消息（始终作用于本轮）
    if let Some(instr) = &req.instructions {
        let text = instr.flatten();
        if !text.trim().is_empty() {
            messages.push(text_message("system", text));
        }
    }

    messages.extend(history);

    match &req.input {
        Some(Input::Text(text)) => {
            messages.push(text_message("user", text.clone()));
        }
        Some(Input::Items(items)) => {
            messages.extend(items_to_messages(items));
        }
        None => {}
    }

    if messages.is_empty() {
        return Err("缺少必填字段 'input'".to_string());
    }

    let tools = convert_tools(req.tools.as_deref());
    let tool_choice = req.tool_choice.as_ref().and_then(convert_tool_choice);
    let parallel_tool_calls = req.parallel_tool_calls;
    let reasoning_effort = convert_reasoning(req.reasoning.as_ref());
    let response_format = convert_text_format(req.text.as_ref());

    // web_search 工具 → search 模式（与 Chat Completions 的 web_search_options 等价）
    let web_search_options =
        has_web_search_tool(req.tools.as_deref()).then_some(WebSearchOptions {
            search_context_size: None,
            user_location: None,
        });

    Ok(ChatCompletionsRequest {
        model: req.model.clone(),
        messages,
        stream: req.stream,
        max_tokens: req.max_output_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        tools,
        tool_choice,
        parallel_tool_calls,
        reasoning_effort,
        response_format,
        web_search_options,
        stream_options: Some(StreamOptions {
            include_usage: true,
            include_obfuscation: req
                .stream_options
                .as_ref()
                .and_then(|o| o.include_obfuscation)
                .unwrap_or(true),
        }),
        metadata: req.metadata.clone(),
        user: req.user.clone(),
        store: req.store,
        // Responses API 未定义、或本适配层不消费的字段
        audio: None,
        frequency_penalty: None,
        function_call: None,
        functions: None,
        logit_bias: None,
        logprobs: None,
        max_completion_tokens: None,
        modalities: None,
        n: None,
        prediction: None,
        presence_penalty: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        safety_identifier: None,
        seed: None,
        service_tier: None,
        stop: None,
        top_logprobs: None,
        verbosity: None,
        _extra: serde_json::Value::Null,
    })
}

fn text_message(role: &str, text: String) -> Message {
    Message {
        role: role.to_string(),
        content: Some(OaiMessageContent::Text(text)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        function_call: None,
        audio: None,
        refusal: None,
    }
}

/// 将输入项数组转换为 Chat Completions 消息列表
fn items_to_messages(items: &[InputItem]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();

    for item in items {
        match item {
            InputItem::Message { role, content } => {
                let role = normalize_role(role);
                messages.push(Message {
                    role,
                    content: Some(content_to_oai(content)),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    function_call: None,
                    audio: None,
                    refusal: None,
                });
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                // 与前一条 assistant 消息合并 tool_calls，避免产生空 assistant 消息
                let call = ToolCall {
                    id: call_id.clone(),
                    ty: "function".to_string(),
                    function: Some(FunctionCall {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }),
                    custom: None,
                    index: 0,
                };
                match messages.last_mut() {
                    Some(last) if last.role == "assistant" && last.tool_calls.is_some() => {
                        last.tool_calls.as_mut().unwrap().push(call);
                    }
                    _ => messages.push(Message {
                        role: "assistant".to_string(),
                        content: None,
                        name: None,
                        tool_call_id: None,
                        tool_calls: Some(vec![call]),
                        function_call: None,
                        audio: None,
                        refusal: None,
                    }),
                }
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                messages.push(Message {
                    role: "tool".to_string(),
                    content: Some(OaiMessageContent::Text(output.clone())),
                    name: None,
                    tool_call_id: Some(call_id.clone()),
                    tool_calls: None,
                    function_call: None,
                    audio: None,
                    refusal: None,
                });
            }
            // item_reference / reasoning / 服务器工具项：本适配层不消费
            InputItem::ItemReference { .. } | InputItem::Other => {}
        }
    }

    messages
}

/// Responses API 允许 `developer` 角色；DeepSeek 侧无对应标签，降级为 system
fn normalize_role(role: &str) -> String {
    match role {
        "developer" => "system".to_string(),
        other => other.to_string(),
    }
}

fn content_to_oai(content: &InputContent) -> OaiMessageContent {
    match content {
        InputContent::Text(t) => OaiMessageContent::Text(t.clone()),
        InputContent::Parts(parts) => {
            // 纯文本块合并为字符串，保留与其他模态并存时的 parts 形态
            let has_non_text = parts
                .iter()
                .any(|p| !matches!(p, InputContentPart::Text(_)));
            if !has_non_text {
                let joined = parts
                    .iter()
                    .filter_map(|p| match p {
                        InputContentPart::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return OaiMessageContent::Text(joined);
            }

            let oai_parts = parts
                .iter()
                .map(|part| match part {
                    InputContentPart::Text(text) => ContentPart {
                        ty: "text".to_string(),
                        text: Some(text.clone()),
                        image_url: None,
                        input_audio: None,
                        file: None,
                        refusal: None,
                    },
                    InputContentPart::ImageUrl { url, detail } => ContentPart {
                        ty: "image_url".to_string(),
                        text: None,
                        image_url: Some(ImageUrlContent {
                            url: url.clone(),
                            detail: detail.clone(),
                        }),
                        input_audio: None,
                        file: None,
                        refusal: None,
                    },
                    InputContentPart::File {
                        file_data,
                        file_id,
                        filename,
                    } => ContentPart {
                        ty: "file".to_string(),
                        text: None,
                        image_url: None,
                        input_audio: None,
                        file: Some(FileContent {
                            file_data: file_data.clone(),
                            file_id: file_id.clone(),
                            filename: filename.clone(),
                        }),
                        refusal: None,
                    },
                })
                .collect();

            OaiMessageContent::Parts(oai_parts)
        }
    }
}

/// 工具名是否合法（DeepSeek 侧以文本注入，过长的名字会污染提示词）
const MAX_TOOL_NAME_LEN: usize = 128;

fn convert_tools(tools: Option<&[ResponsesTool]>) -> Option<Vec<Tool>> {
    let tools = tools?;
    let converted: Vec<Tool> = tools
        .iter()
        .filter_map(|tool| {
            if tool.ty != "function" && tool.ty != "custom" {
                // 内置工具（web_search_preview / file_search / mcp / code_interpreter…）
                // 由调用方通过 search 模式近似，不注入函数定义
                return None;
            }

            // 扁平结构优先；缺失时回退到 Chat Completions 的嵌套结构
            let (name, description, parameters, strict) = match &tool.function {
                Some(nested) => (
                    nested.name.clone(),
                    nested.description.clone(),
                    nested.parameters.clone(),
                    nested.strict,
                ),
                None => (
                    tool.name.clone()?,
                    tool.description.clone(),
                    tool.parameters.clone().unwrap_or_default(),
                    tool.strict,
                ),
            };

            if name.is_empty() || name.len() > MAX_TOOL_NAME_LEN {
                return None;
            }

            if tool.ty == "custom" {
                return Some(Tool {
                    ty: "custom".to_string(),
                    function: None,
                    custom: Some(CustomTool {
                        name,
                        description,
                        format: None,
                    }),
                });
            }

            Some(Tool {
                ty: "function".to_string(),
                function: Some(FunctionDefinition {
                    name,
                    description,
                    parameters,
                    strict,
                }),
                custom: None,
            })
        })
        .collect();

    (!converted.is_empty()).then_some(converted)
}

fn has_web_search_tool(tools: Option<&[ResponsesTool]>) -> bool {
    tools.is_some_and(|tools| {
        tools.iter().any(|t| {
            t.ty.starts_with("web_search") || t.ty == "web_search_preview" || t.ty == "web_search"
        })
    })
}

fn convert_tool_choice(choice: &serde_json::Value) -> Option<ToolChoice> {
    match choice {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" | "none" | "required" => Some(ToolChoice::Mode(s.clone())),
            _ => None,
        },
        serde_json::Value::Object(obj) => {
            let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            match ty {
                "function" => {
                    let name = obj.get("name").and_then(|v| v.as_str())?;
                    Some(ToolChoice::Named(NamedToolChoice {
                        ty: "function".to_string(),
                        function: NamedFunction {
                            name: name.to_string(),
                        },
                    }))
                }
                "custom" => {
                    let name = obj.get("name").and_then(|v| v.as_str())?;
                    Some(ToolChoice::Custom(NamedCustomChoice {
                        ty: "custom".to_string(),
                        custom: NamedCustom {
                            name: name.to_string(),
                        },
                    }))
                }
                "allowed_tools" => {
                    let mode = obj
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .unwrap_or("auto")
                        .to_string();
                    let tools = obj.get("tools").and_then(|v| v.as_array()).cloned();
                    Some(ToolChoice::AllowedTools(
                        crate::openai_adapter::types::AllowedToolsChoice {
                            ty: "allowed_tools".to_string(),
                            allowed_tools: crate::openai_adapter::types::AllowedTools {
                                mode,
                                tools,
                            },
                        },
                    ))
                }
                // web_search_preview 等内置工具选择：不注入工具指令
                _ => None,
            }
        }
        _ => None,
    }
}

/// `reasoning.effort` → `reasoning_effort`
///
/// 未显式给出 effort 时返回 `None`，由 openai_adapter 的默认值（`"high"`）接管。
fn convert_reasoning(reasoning: Option<&ReasoningParam>) -> Option<String> {
    reasoning.and_then(|r| r.effort.clone())
}

/// `text.format` → `response_format`
fn convert_text_format(text: Option<&super::types::TextParam>) -> Option<ResponseFormat> {
    let format: &TextFormat = text?.format.as_ref()?;
    match format.ty.as_str() {
        "json_object" => Some(ResponseFormat {
            ty: "json_object".to_string(),
            json_schema: None,
        }),
        "json_schema" => {
            // Responses 的扁平 json_schema → Chat Completions 的嵌套形态
            let mut schema = serde_json::Map::new();
            if let Some(name) = &format.name {
                schema.insert("name".to_string(), serde_json::Value::String(name.clone()));
            }
            if let Some(s) = &format.schema {
                schema.insert("schema".to_string(), s.clone());
            }
            if let Some(strict) = format.strict {
                schema.insert("strict".to_string(), serde_json::Value::Bool(strict));
            }
            Some(ResponseFormat {
                ty: "json_schema".to_string(),
                json_schema: Some(serde_json::Value::Object(schema)),
            })
        }
        // `text` 或未知格式：不注入任何格式约束
        _ => None,
    }
}

/// 取出响应回显用的 system 指令文本
#[must_use]
pub(crate) fn instructions_text(instructions: Option<&Instructions>) -> Option<String> {
    instructions.map(Instructions::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(json: &str) -> Result<ChatCompletionsRequest, String> {
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        into_chat_completions(&req, Vec::new())
    }

    #[test]
    fn string_input_becomes_user_message() {
        let req = convert(r#"{"model":"deepseek-default","input":"hello"}"#).unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text("hello".to_string()))
        );
    }

    #[test]
    fn instructions_become_system_message() {
        let req = convert(r#"{"model":"m","instructions":"be nice","input":"hi"}"#).unwrap();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn history_precedes_new_input_and_instructions_win() {
        let req: ResponsesRequest =
            serde_json::from_str(r#"{"model":"m","instructions":"new sys","input":"second"}"#)
                .unwrap();
        let history = vec![
            text_message("system", "old sys".to_string()),
            text_message("user", "first".to_string()),
            text_message("assistant", "answer".to_string()),
        ];
        let chat = into_chat_completions(&req, history).unwrap();
        let roles: Vec<&str> = chat.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "system", "user", "assistant", "user"]);
    }

    #[test]
    fn developer_role_downgraded_to_system() {
        let req = convert(r#"{"model":"m","input":[{"role":"developer","content":"x"}]}"#).unwrap();
        assert_eq!(req.messages[0].role, "system");
    }

    #[test]
    fn function_call_items_fold_into_assistant_tool_calls() {
        let req = convert(
            r#"{"model":"m","input":[
                {"role":"user","content":"weather?"},
                {"type":"function_call","call_id":"call_1","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"sunny"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[1].role, "assistant");
        let calls = req.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(req.messages[2].role, "tool");
        assert_eq!(req.messages[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn consecutive_function_calls_share_one_assistant_message() {
        let req = convert(
            r#"{"model":"m","input":[
                {"type":"function_call","call_id":"c1","name":"a","arguments":"{}"},
                {"type":"function_call","call_id":"c2","name":"b","arguments":"{}"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].tool_calls.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn flat_function_tool_converted() {
        let req = convert(
            r#"{"model":"m","input":"hi","tools":[{"type":"function","name":"f","description":"d","parameters":{"type":"object"},"strict":true}]}"#,
        )
        .unwrap();
        let tools = req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        let f = tools[0].function.as_ref().unwrap();
        assert_eq!(f.name, "f");
        assert_eq!(f.strict, Some(true));
    }

    #[test]
    fn web_search_tool_enables_search_mode() {
        let req = convert(r#"{"model":"m","input":"hi","tools":[{"type":"web_search_preview"}]}"#)
            .unwrap();
        assert!(req.web_search_options.is_some());
        assert!(req.tools.is_none());
    }

    #[test]
    fn tool_choice_variants() {
        let named = convert(
            r#"{"model":"m","input":"hi","tools":[{"type":"function","name":"f"}],"tool_choice":{"type":"function","name":"f"}}"#,
        )
        .unwrap();
        match named.tool_choice {
            Some(ToolChoice::Named(n)) => assert_eq!(n.function.name, "f"),
            other => panic!("expected Named, got {other:?}"),
        }

        let mode = convert(r#"{"model":"m","input":"hi","tool_choice":"required"}"#).unwrap();
        match mode.tool_choice {
            Some(ToolChoice::Mode(m)) => assert_eq!(m, "required"),
            other => panic!("expected Mode, got {other:?}"),
        }
    }

    #[test]
    fn text_format_json_schema_becomes_response_format() {
        let req = convert(
            r#"{"model":"m","input":"hi","text":{"format":{"type":"json_schema","name":"out","schema":{"type":"object"},"strict":true}}}"#,
        )
        .unwrap();
        let rf = req.response_format.unwrap();
        assert_eq!(rf.ty, "json_schema");
        let schema = rf.json_schema.unwrap();
        assert_eq!(schema["name"], "out");
        assert_eq!(schema["strict"], true);
    }

    #[test]
    fn text_format_plain_text_ignored() {
        let req =
            convert(r#"{"model":"m","input":"hi","text":{"format":{"type":"text"}}}"#).unwrap();
        assert!(req.response_format.is_none());
    }

    #[test]
    fn reasoning_effort_mapped() {
        let req = convert(r#"{"model":"m","input":"hi","reasoning":{"effort":"low"}}"#).unwrap();
        assert_eq!(req.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn reasoning_effort_absent_leaves_default_to_adapter() {
        let req = convert(r#"{"model":"m","input":"hi"}"#).unwrap();
        assert!(req.reasoning_effort.is_none());
    }

    #[test]
    fn max_output_tokens_mapped_to_max_tokens() {
        let req = convert(r#"{"model":"m","input":"hi","max_output_tokens":512}"#).unwrap();
        assert_eq!(req.max_tokens, Some(512));
    }

    #[test]
    fn empty_input_rejected() {
        let err = convert(r#"{"model":"m"}"#).unwrap_err();
        assert!(err.contains("input"), "unexpected error: {err}");
    }

    #[test]
    fn image_part_preserved() {
        let req = convert(
            r#"{"model":"m","input":[{"role":"user","content":[
                {"type":"input_text","text":"what is this"},
                {"type":"input_image","image_url":"https://example.com/a.png"}
            ]}]}"#,
        )
        .unwrap();
        match req.messages[0].content.as_ref().unwrap() {
            OaiMessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].ty, "text");
                assert_eq!(parts[1].ty, "image_url");
                assert_eq!(
                    parts[1].image_url.as_ref().unwrap().url,
                    "https://example.com/a.png"
                );
            }
            other @ OaiMessageContent::Text(_) => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_parts_joined_without_parts_wrapper() {
        let req = convert(
            r#"{"model":"m","input":[{"role":"user","content":[{"type":"input_text","text":"a"},{"type":"input_text","text":"b"}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text("a\nb".to_string()))
        );
    }

    #[test]
    fn tool_names_too_long_dropped() {
        let long_name = "x".repeat(200);
        let json = format!(
            r#"{{"model":"m","input":"hi","tools":[{{"type":"function","name":"{long_name}"}}]}}"#
        );
        let req = convert(&json).unwrap();
        assert!(req.tools.is_none());
    }
}

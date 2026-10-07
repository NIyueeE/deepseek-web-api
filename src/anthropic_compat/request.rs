//! Anthropic 请求映射 —— MessagesRequest → ChatCompletionsRequest 结构体转换
//!
//! 纯函数：不接受 JSON 字节，直接做结构体到结构体的字段映射。

use crate::anthropic_compat::types::{
    ContentBlock, ImageSource, MessageContent, MessageParam, MessagesRequest, SystemContent,
    ThinkingConfig, ToolChoice, ToolResultContent, ToolUnion,
};
use crate::openai_adapter::types::{
    ChatCompletionsRequest, ContentPart, FileContent, FunctionCall, FunctionDefinition,
    ImageUrlContent, Message, MessageContent as OaiMessageContent, NamedFunction, NamedToolChoice,
    ResponseFormat, StopSequence, StreamOptions, Tool, ToolCall, ToolChoice as OaiToolChoice,
};

// ============================================================================
// 映射函数
// ============================================================================

/// 将 Anthropic MessagesRequest 直接映射为 ChatCompletionsRequest 结构体
pub(crate) fn into_chat_completions(req: MessagesRequest) -> ChatCompletionsRequest {
    // messages: system 前置 + messages 转换
    let mut messages = Vec::new();
    if let Some(ref system) = req.system {
        messages.push(system_to_message(system));
    }
    for msg in &req.messages {
        messages.extend(message_param_to_messages(msg));
    }

    // tools + parallel_tool_calls
    let (tools, parallel_tool_calls) = convert_tools_and_choice(&req);

    // thinking → reasoning_effort
    let reasoning_effort = req.thinking.map(|t| match t {
        ThinkingConfig::Enabled { .. } | ThinkingConfig::Adaptive { .. } => "high".to_string(),
        ThinkingConfig::Disabled => "none".to_string(),
    });

    // output_config.format → response_format
    let response_format = req
        .output_config
        .and_then(|oc| oc.format)
        .map(|fmt| ResponseFormat {
            ty: "json_schema".to_string(),
            json_schema: Some(fmt.schema),
        });

    // web_search_options
    let web_search_options = req
        .web_search_options
        .and_then(|v| serde_json::from_value(v).ok());

    ChatCompletionsRequest {
        model: req.model,
        messages,
        stream: req.stream,
        max_tokens: Some(req.max_tokens),
        stop: req
            .stop_sequences
            .filter(|s| !s.is_empty())
            .map(StopSequence::Multiple),
        temperature: req.temperature,
        top_p: req.top_p,
        tools,
        tool_choice: req.tool_choice.map(|tc| convert_tool_choice(&tc)),
        parallel_tool_calls,
        reasoning_effort,
        response_format,
        web_search_options,
        // 其余字段保持默认
        audio: None,
        frequency_penalty: None,
        function_call: None,
        functions: None,
        logit_bias: None,
        logprobs: None,
        max_completion_tokens: None,
        metadata: None,
        modalities: None,
        n: None,
        prediction: None,
        presence_penalty: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        safety_identifier: None,
        seed: None,
        service_tier: None,
        store: None,
        stream_options: Some(StreamOptions {
            include_usage: true,
            include_obfuscation: true,
        }),
        top_logprobs: None,
        user: None,
        verbosity: None,
        _extra: serde_json::Value::default(),
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

const fn empty_message(role: String, content: OaiMessageContent) -> Message {
    Message {
        role,
        content: Some(content),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        function_call: None,
        audio: None,
        refusal: None,
    }
}

fn system_to_message(system: &SystemContent) -> Message {
    let text = match system {
        SystemContent::Text(t) => t.clone(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    empty_message("system".to_string(), OaiMessageContent::Text(text))
}

fn message_param_to_messages(msg: &MessageParam) -> Vec<Message> {
    let blocks = match &msg.content {
        MessageContent::Text(t) => {
            return vec![empty_message(
                msg.role.clone(),
                OaiMessageContent::Text(t.clone()),
            )];
        }
        MessageContent::Blocks(b) => b,
    };

    match msg.role.as_str() {
        "assistant" => assistant_blocks_to_messages(blocks),
        "user" => user_blocks_to_messages(blocks),
        _ => {
            let text = extract_text_from_blocks(blocks);
            vec![empty_message(
                msg.role.clone(),
                OaiMessageContent::Text(text),
            )]
        }
    }
}

/// 将 assistant 的 content blocks 映射为 OpenAI 消息
fn assistant_blocks_to_messages(blocks: &[ContentBlock]) -> Vec<Message> {
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => texts.push(text.clone()),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    ty: "function".to_string(),
                    function: Some(FunctionCall {
                        name: name.clone(),
                        arguments: input.to_string(),
                    }),
                    custom: None,
                    index: 0,
                });
            }
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Other => {}
        }
    }

    let content = if texts.is_empty() {
        None
    } else {
        Some(OaiMessageContent::Text(texts.join("\n")))
    };

    vec![Message {
        role: "assistant".to_string(),
        content,
        name: None,
        tool_call_id: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        function_call: None,
        audio: None,
        refusal: None,
    }]
}

struct FilePart {
    data_url: String,
    filename: String,
}

fn infer_doc_filename(mime: &str) -> String {
    let ext = match mime {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/json" => "json",
        "application/zip" => "zip",
        "application/xml" => "xml",
        "text/csv" => "csv",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        _ => "doc",
    };
    format!("document.{ext}")
}

/// 将 user 的 content blocks 映射为 OpenAI 消息
fn user_blocks_to_messages(blocks: &[ContentBlock]) -> Vec<Message> {
    let mut text_parts = Vec::new();
    let mut image_parts = Vec::new();
    let mut file_parts: Vec<FilePart> = Vec::new();
    let mut tool_results = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            ContentBlock::Image { source } => {
                let url = match source {
                    ImageSource::Base64 { data, media_type } => {
                        format!("data:{media_type};base64,{data}")
                    }
                    ImageSource::Url { url } => url.clone(),
                };
                image_parts.push(url);
            }
            ContentBlock::Document { source, title } => match source {
                ImageSource::Base64 { data, media_type } => {
                    let data_url = format!("data:{media_type};base64,{data}");
                    let filename = infer_doc_filename(media_type);
                    let desc = title
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .unwrap_or(&filename);
                    text_parts.push(format!("[文件: {desc}]"));
                    file_parts.push(FilePart { data_url, filename });
                }
                ImageSource::Url { url } => {
                    // 利用 image_url part + HTTP URL 触发搜索模式
                    // format_part 会输出 [请访问这个链接: {url}]
                    image_parts.push(url.clone());
                }
            },
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                let text = match content {
                    Some(ToolResultContent::Text(t)) => t.clone(),
                    Some(ToolResultContent::Blocks(b)) => extract_text_from_blocks(b),
                    None => String::new(),
                };
                tool_results.push(Message {
                    role: "tool".to_string(),
                    content: Some(OaiMessageContent::Text(text)),
                    name: None,
                    tool_call_id: Some(tool_use_id.clone()),
                    tool_calls: None,
                    function_call: None,
                    audio: None,
                    refusal: None,
                });
            }
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::Other => {}
        }
    }

    let mut result = Vec::new();

    // 文本 + 图片 + 文件合并为一个 user message
    if !text_parts.is_empty() || !image_parts.is_empty() || !file_parts.is_empty() {
        if image_parts.is_empty() && file_parts.is_empty() {
            result.push(empty_message(
                "user".to_string(),
                OaiMessageContent::Text(text_parts.join("\n")),
            ));
        } else {
            // 包含图片：使用 parts 数组
            let mut parts = Vec::new();
            for text in &text_parts {
                parts.push(ContentPart {
                    ty: "text".to_string(),
                    text: Some(text.clone()),
                    image_url: None,
                    input_audio: None,
                    file: None,
                    refusal: None,
                });
            }
            for url in &image_parts {
                parts.push(ContentPart {
                    ty: "image_url".to_string(),
                    text: None,
                    image_url: Some(ImageUrlContent {
                        url: url.clone(),
                        detail: None,
                    }),
                    input_audio: None,
                    file: None,
                    refusal: None,
                });
            }
            for fp in &file_parts {
                parts.push(ContentPart {
                    ty: "file".to_string(),
                    text: None,
                    image_url: None,
                    input_audio: None,
                    file: Some(FileContent {
                        file_data: Some(fp.data_url.clone()),
                        file_id: None,
                        filename: Some(fp.filename.clone()),
                    }),
                    refusal: None,
                });
            }
            result.push(Message {
                role: "user".to_string(),
                content: Some(OaiMessageContent::Parts(parts)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                function_call: None,
                audio: None,
                refusal: None,
            });
        }
    }

    // tool_result 作为独立的 tool role messages
    result.extend(tool_results);

    result
}

fn extract_text_from_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn convert_tools_and_choice(req: &MessagesRequest) -> (Option<Vec<Tool>>, Option<bool>) {
    let tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .filter_map(|tool| match tool {
                ToolUnion::Custom {
                    name,
                    description,
                    input_schema,
                    strict,
                } => Some(Tool {
                    ty: "function".to_string(),
                    function: Some(FunctionDefinition {
                        name: name.clone(),
                        description: Some(description.as_deref().unwrap_or("").to_string()),
                        parameters: input_schema.clone(),
                        strict: *strict,
                    }),
                    custom: None,
                }),
                ToolUnion::Other => None,
            })
            .collect()
    });

    let disable_parallel = req
        .tool_choice
        .as_ref()
        .is_some_and(|tc| tc.disable_parallel());

    let parallel_tool_calls = disable_parallel.then_some(false);

    (tools, parallel_tool_calls)
}

fn convert_tool_choice(tc: &ToolChoice) -> OaiToolChoice {
    match tc {
        ToolChoice::Auto { .. } => OaiToolChoice::Mode("auto".to_string()),
        ToolChoice::Any { .. } => OaiToolChoice::Mode("required".to_string()),
        ToolChoice::Tool { name, .. } => OaiToolChoice::Named(NamedToolChoice {
            ty: "function".to_string(),
            function: NamedFunction { name: name.clone() },
        }),
        ToolChoice::None => OaiToolChoice::Mode("none".to_string()),
    }
}

impl ToolChoice {
    const fn disable_parallel(&self) -> bool {
        match self {
            ToolChoice::Auto {
                disable_parallel_tool_use,
            }
            | ToolChoice::Any {
                disable_parallel_tool_use,
            }
            | ToolChoice::Tool {
                disable_parallel_tool_use,
                ..
            } => *disable_parallel_tool_use,
            ToolChoice::None => false,
        }
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(body: &[u8]) -> ChatCompletionsRequest {
        into_chat_completions(serde_json::from_slice(body).unwrap())
    }

    #[test]
    fn basic_user_message() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 1024
        }"#;

        let req = convert(body);
        assert_eq!(req.model, "deepseek-default");
        assert_eq!(req.max_tokens, Some(1024));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text("Hello".to_string()))
        );
    }

    #[test]
    fn system_as_string() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "system": "You are a helpful assistant."
        }"#;

        let req = convert(body);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text(
                "You are a helpful assistant.".to_string()
            ))
        );
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn system_as_blocks() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "Sys1"}, {"type": "text", "text": "Sys2"}]
        }"#;

        let req = convert(body);
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text("Sys1\nSys2".to_string()))
        );
    }

    #[test]
    fn user_with_text_blocks() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}, {"type": "text", "text": "World"}]}
            ],
            "max_tokens": 1024
        }"#;

        let req = convert(body);
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text("Hello\nWorld".to_string()))
        );
    }

    #[test]
    fn assistant_with_tool_use() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me check"},
                        {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "Beijing"}}
                    ]
                }
            ],
            "max_tokens": 1024
        }"#;

        let req = convert(body);
        let msg = &req.messages[0];
        assert_eq!(msg.role, "assistant");
        assert_eq!(
            msg.content,
            Some(OaiMessageContent::Text("Let me check".to_string()))
        );
        let tool_calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_01");
        assert_eq!(tool_calls[0].ty, "function");
        assert_eq!(tool_calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            tool_calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"Beijing"}"#
        );
    }

    #[test]
    fn user_with_tool_result() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "25C"}
                    ]
                }
            ],
            "max_tokens": 1024
        }"#;

        let req = convert(body);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "tool");
        assert_eq!(req.messages[0].tool_call_id, Some("toolu_01".to_string()));
        assert_eq!(
            req.messages[0].content,
            Some(OaiMessageContent::Text("25C".to_string()))
        );
    }

    #[test]
    fn stream_and_stop_sequences() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "stream": true,
            "stop_sequences": ["STOP", "HALT"],
            "temperature": 0.7,
            "top_p": 0.9
        }"#;

        let req = convert(body);
        assert!(req.stream);
        assert_eq!(
            req.stop,
            Some(StopSequence::Multiple(vec![
                "STOP".to_string(),
                "HALT".to_string()
            ]))
        );
        assert!((req.temperature.unwrap() - 0.7).abs() < 0.001);
        assert!((req.top_p.unwrap() - 0.9).abs() < 0.001);
    }

    #[test]
    fn tools_mapping() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "tools": [
                {
                    "type": "custom",
                    "name": "get_weather",
                    "description": "Get weather",
                    "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            ],
            "tool_choice": {"type": "auto"}
        }"#;

        let req = convert(body);
        let tools = req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].ty, "function");
        assert_eq!(tools[0].function.as_ref().unwrap().name, "get_weather");
        assert!(matches!(
            req.tool_choice,
            Some(OaiToolChoice::Mode(ref m)) if m == "auto"
        ));
    }

    #[test]
    fn tool_choice_named_tool() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "tools": [{"type": "custom", "name": "get_weather", "input_schema": {}}],
            "tool_choice": {"type": "tool", "name": "get_weather"}
        }"#;

        let req = convert(body);
        match req.tool_choice {
            Some(OaiToolChoice::Named(ref nc)) => {
                assert_eq!(nc.ty, "function");
                assert_eq!(nc.function.name, "get_weather");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn tool_choice_disable_parallel() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "tools": [{"type": "custom", "name": "f", "input_schema": {}}],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true}
        }"#;

        let req = convert(body);
        assert_eq!(req.parallel_tool_calls, Some(false));
    }

    #[test]
    fn thinking_enabled() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "thinking": {"type": "enabled", "budget_tokens": 2048}
        }"#;

        let req = convert(body);
        assert_eq!(req.reasoning_effort, Some("high".to_string()));
    }

    #[test]
    fn thinking_disabled() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "thinking": {"type": "disabled"}
        }"#;

        let req = convert(body);
        assert_eq!(req.reasoning_effort, Some("none".to_string()));
    }

    #[test]
    fn output_config_json_schema() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}}
        }"#;

        let req = convert(body);
        let rf = req.response_format.as_ref().unwrap();
        assert_eq!(rf.ty, "json_schema");
        assert_eq!(rf.json_schema.as_ref().unwrap()["type"], "object");
    }

    #[test]
    fn document_url_mapped_to_image_url() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Hello"},
                        {"type": "document", "source": {"type": "url", "url": "http://example.com/doc"}}
                    ]
                }
            ],
            "max_tokens": 1024
        }"#;

        let req = convert(body);
        match &req.messages[0].content {
            Some(OaiMessageContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].ty, "text");
                assert_eq!(parts[0].text.as_deref(), Some("Hello"));
                assert_eq!(parts[1].ty, "image_url");
                assert_eq!(
                    parts[1].image_url.as_ref().unwrap().url,
                    "http://example.com/doc"
                );
            }
            other => panic!("expected Parts, got {other:?}"),
        }
    }

    #[test]
    fn server_tools_ignored() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
            "tools": [
                {"type": "custom", "name": "my_tool", "input_schema": {}},
                {"type": "bash_20250124", "name": "bash"}
            ]
        }"#;

        let req = convert(body);
        let tools = req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.as_ref().unwrap().name, "my_tool");
    }

    #[test]
    fn image_source_mapped() {
        // base64 和 url 两种 image source 都映射为 image_url content part
        let cases = [
            (
                r#"{"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "abc123"}}"#,
                "data:image/jpeg;base64,abc123",
            ),
            (
                r#"{"type": "image", "source": {"type": "url", "url": "https://example.com/img.jpg"}}"#,
                "https://example.com/img.jpg",
            ),
        ];
        for (image_block, expected_url) in cases {
            let body = format!(
                r#"{{"model":"deepseek-default","messages":[{{"role":"user","content":[{{"type":"text","text":"Describe this"}},{image_block}]}}],"max_tokens":1024}}"#
            );
            let req = convert(body.as_bytes());
            let parts = match &req.messages[0].content {
                Some(OaiMessageContent::Parts(parts)) => parts,
                other => panic!("expected Parts, got {other:?}"),
            };
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[1].image_url.as_ref().unwrap().url, expected_url);
        }
    }

    #[test]
    fn web_search_options_mapped() {
        let body = br#"{
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "latest news"}],
            "max_tokens": 1024,
            "web_search_options": {"search_context_size": "high"}
        }"#;

        let req = convert(body);
        let opts = req.web_search_options.as_ref().unwrap();
        assert_eq!(opts.search_context_size, Some("high".to_string()));
    }
}

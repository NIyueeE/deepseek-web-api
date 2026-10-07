//! 非流式响应映射 —— 将 ChatCompletionsResponse 映射为 MessagesResponse

use log::debug;

use super::{ContentBlock, finish_reason_map, map_id};
use crate::anthropic_compat::types::{MessagesResponse, Usage};
use crate::openai_adapter::types::{ChatCompletionsResponse, ToolCall};

/// 将 OpenAI ChatCompletionsResponse 直接映射为 MessagesResponse
pub fn from_chat_completions(resp: &ChatCompletionsResponse) -> MessagesResponse {
    debug!(target: "anthropic_compat::response::aggregate", "mapping non-streaming response");
    let choice = resp.choices.first();
    let message = choice.map(|c| &c.message);

    let mut content: Vec<ContentBlock> = Vec::new();

    if let Some(msg) = message {
        if let Some(ref thinking) = msg.reasoning_content
            && !thinking.is_empty()
        {
            content.push(ContentBlock::Thinking {
                thinking: thinking.clone(),
                signature: String::new(),
            });
        }
        if let Some(ref text) = msg.content
            && !text.is_empty()
        {
            content.push(ContentBlock::Text { text: text.clone() });
        }
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                let input = parse_tool_call_input(call);
                content.push(ContentBlock::ToolUse {
                    id: map_id(&call.id),
                    name: tool_call_name(call),
                    input,
                });
            }
        }
    }

    // Anthropic 的非流式响应 `stop_reason` 必须非空
    let stop_reason = choice
        .and_then(|c| c.finish_reason)
        .map(finish_reason_map)
        .or_else(|| Some("end_turn".to_string()));

    let usage = resp.usage.as_ref().map_or(
        Usage {
            input_tokens: 0,
            output_tokens: 0,
        },
        |u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        },
    );

    debug!(target: "anthropic_compat::response::aggregate", "mapping done: content_blocks={}", content.len());
    MessagesResponse {
        id: map_id(&resp.id),
        ty: "message",
        role: "assistant",
        model: resp.model.clone(),
        content,
        stop_reason,
        stop_sequence: None,
        usage,
    }
}

fn tool_call_name(call: &ToolCall) -> String {
    call.function
        .as_ref()
        .map(|f| f.name.clone())
        .or_else(|| call.custom.as_ref().map(|c| c.name.clone()))
        .unwrap_or_default()
}

fn parse_tool_call_input(call: &ToolCall) -> serde_json::Value {
    call.function
        .as_ref()
        .and_then(|f| serde_json::from_str(&f.arguments).ok())
        .or_else(|| call.custom.as_ref().and_then(|c| c.input.clone()))
        .unwrap_or_else(|| serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use crate::anthropic_compat::types::ResponseContentBlock;
    use crate::openai_adapter::types::{
        ChatCompletionsResponse, Choice, FunctionCall, MessageResponse, ToolCall, Usage,
    };

    use super::*;

    fn resp(
        id: &str,
        model: &str,
        content: Option<&str>,
        reasoning: Option<&str>,
        tool_calls: Option<Vec<ToolCall>>,
        finish_reason: Option<&'static str>,
        usage: Option<Usage>,
    ) -> ChatCompletionsResponse {
        let message_content = match content {
            Some(c) if !c.is_empty() => Some(c.to_string()),
            _ => None,
        };
        ChatCompletionsResponse {
            id: id.to_string(),
            object: "chat.completion",
            created: 1_713_700_000,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: MessageResponse {
                    role: "assistant",
                    content: message_content,
                    reasoning_content: reasoning.map(|s| s.to_string()),
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls,
                },
                finish_reason,
                logprobs: None,
            }],
            usage,
            service_tier: None,
            system_fingerprint: None,
        }
    }

    #[test]
    fn openai_finish_reasons_map_to_valid_anthropic_values() {
        // Anthropic StopReason 只允许
        // end_turn | max_tokens | stop_sequence | tool_use | pause_turn | refusal
        // | model_context_window_exceeded
        let allowed = [
            "end_turn",
            "max_tokens",
            "stop_sequence",
            "tool_use",
            "pause_turn",
            "refusal",
            "model_context_window_exceeded",
        ];
        for (openai, expected) in [
            ("stop", "end_turn"),
            ("tool_calls", "tool_use"),
            ("length", "max_tokens"),
            ("content_filter", "refusal"),
            ("function_call", "tool_use"),
        ] {
            let mapped = finish_reason_map(openai);
            assert_eq!(mapped, expected, "{openai} 应映射为 {expected}");
            assert!(
                allowed.contains(&mapped.as_str()),
                "{mapped} 不是合法 stop_reason"
            );
        }
        // 未知取值不得透传
        assert_eq!(finish_reason_map("weird"), "end_turn");
    }

    #[test]
    fn length_finish_reason_maps_to_max_tokens() {
        let r = resp("x", "m", Some("cut"), None, None, Some("length"), None);
        let msg = from_chat_completions(&r);
        assert_eq!(msg.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn plain_text_response() {
        let r = resp(
            "chatcmpl-1",
            "deepseek-default",
            Some("hello world"),
            None,
            None,
            Some("stop"),
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        );
        let msg = from_chat_completions(&r);
        assert_eq!(msg.ty, "message");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.id, "msg_1");
        assert_eq!(msg.model, "deepseek-default");
        assert_eq!(msg.stop_reason, Some("end_turn".to_string()));
        assert_eq!(msg.usage.input_tokens, 10);
        assert_eq!(msg.usage.output_tokens, 5);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(msg.content[0], ResponseContentBlock::Text { .. }));
    }

    #[test]
    fn thinking_and_text_response() {
        let r = resp(
            "chatcmpl-2",
            "deepseek-expert",
            Some("The answer is 42."),
            Some("Let me think..."),
            None,
            Some("stop"),
            Some(Usage {
                prompt_tokens: 20,
                completion_tokens: 10,
                total_tokens: 30,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        );
        let msg = from_chat_completions(&r);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(
            msg.content[0],
            ResponseContentBlock::Thinking { .. }
        ));
        assert!(matches!(msg.content[1], ResponseContentBlock::Text { .. }));
        if let ResponseContentBlock::Thinking { ref thinking, .. } = msg.content[0] {
            assert_eq!(thinking, "Let me think...");
        }
        if let ResponseContentBlock::Text { ref text } = msg.content[1] {
            assert_eq!(text, "The answer is 42.");
        }
    }

    #[test]
    fn tool_calls_with_and_without_text() {
        let make_call = |content: &str, _expected_len: usize| {
            let tool_calls = Some(vec![ToolCall {
                id: "call_abc".to_string(),
                ty: "function".to_string(),
                function: Some(FunctionCall {
                    name: "get_weather".to_string(),
                    arguments: r#"{"city":"Beijing"}"#.to_string(),
                }),
                custom: None,
                index: 0,
            }]);
            let content_opt = if content.is_empty() {
                None
            } else {
                Some(content)
            };
            let r = resp(
                "chatcmpl-x",
                "deepseek-default",
                content_opt,
                None,
                tool_calls,
                Some("tool_calls"),
                Some(Usage {
                    prompt_tokens: 15,
                    completion_tokens: 8,
                    total_tokens: 23,
                    prompt_tokens_details: None,
                    completion_tokens_details: None,
                }),
            );
            from_chat_completions(&r)
        };

        let msg = make_call("", 1);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(
            msg.content[0],
            ResponseContentBlock::ToolUse { .. }
        ));

        let msg = make_call("Let me check the weather", 2);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(msg.content[0], ResponseContentBlock::Text { .. }));
        assert!(matches!(
            msg.content[1],
            ResponseContentBlock::ToolUse { .. }
        ));
        if let ResponseContentBlock::ToolUse { ref input, .. } = msg.content[msg.content.len() - 1]
        {
            assert_eq!(input["city"], "Beijing");
        }
    }

    #[test]
    fn empty_or_missing_content() {
        let r1 = resp(
            "x",
            "m",
            Some(""),
            None,
            None,
            Some("stop"),
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 1,
                total_tokens: 6,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        );
        assert!(from_chat_completions(&r1).content.is_empty());

        let r2 = resp("x", "m", None, None, None, Some("stop"), None);
        assert!(from_chat_completions(&r2).content.is_empty());
    }

    #[test]
    fn malformed_arguments_fallback_to_empty_object() {
        let tool_calls = Some(vec![ToolCall {
            id: "call_bad".to_string(),
            ty: "function".to_string(),
            function: Some(FunctionCall {
                name: "foo".to_string(),
                arguments: "not-json".to_string(),
            }),
            custom: None,
            index: 0,
        }]);
        let r = resp(
            "chatcmpl-7",
            "deepseek-default",
            None,
            None,
            tool_calls,
            Some("tool_calls"),
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                total_tokens: 8,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        );
        let msg = from_chat_completions(&r);
        if let ResponseContentBlock::ToolUse { ref input, .. } = msg.content[0] {
            assert_eq!(input, &serde_json::json!({}));
        }
    }

    #[test]
    fn no_choices_empty_content() {
        let r = ChatCompletionsResponse {
            id: "chatcmpl-empty".to_string(),
            object: "chat.completion",
            created: 0,
            model: "deepseek-default".to_string(),
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            service_tier: None,
            system_fingerprint: None,
        };
        let msg = from_chat_completions(&r);
        assert!(msg.content.is_empty());
        // 无 choices 时没有 finish_reason，退化为 end_turn（Anthropic 要求非空）
        assert_eq!(msg.stop_reason.as_deref(), Some("end_turn"));
    }
}

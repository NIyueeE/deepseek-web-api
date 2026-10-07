//! OpenAI 请求解析 —— 将 OpenAI ChatCompletion 请求降级为 ds_core::ChatRequest
//!
//! 当前限制：
//! - 多轮对话通过 DeepSeek 原生标签格式压缩为单个 prompt 字符串
//! - tool 定义嵌入到最后一个 `<｜Assistant｜>` 后的不闭合 `<think>` 块中

pub(crate) mod files;
pub(crate) mod normalize;
pub(crate) mod prompt;
pub(crate) mod resolver;
pub(crate) mod tools;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_adapter::OpenAIAdapterError;
    use crate::openai_adapter::types::{
        ChatCompletionsRequest, FunctionCallOption, NamedFunction, NamedToolChoice, Tool,
        ToolChoice,
    };

    fn default_registry() -> std::collections::HashMap<String, String> {
        crate::config::DsCoreSection::default().model_registry()
    }

    /// 测试用的 prepare，模拟 adapter 内部的解析逻辑
    #[derive(Debug)]
    struct TestRequest {
        prompt: String,
        thinking_enabled: bool,
        search_enabled: bool,
        stream: bool,
        stop: Vec<String>,
        /// (include_usage, include_obfuscation)
        opts: (bool, bool),
    }

    fn parse_json(val: serde_json::Value) -> Result<TestRequest, OpenAIAdapterError> {
        let mut req: ChatCompletionsRequest = serde_json::from_value(val)
            .map_err(|e| OpenAIAdapterError::BadRequest(format!("bad request: {e}")))?;
        let registry = default_registry();

        if req.tools.as_ref().is_none_or(|t| t.is_empty())
            && let Some(functions) = req.functions.clone()
            && !functions.is_empty()
        {
            req.tools = Some(
                functions
                    .into_iter()
                    .map(|f| Tool {
                        ty: "function".to_string(),
                        function: Some(f),
                        custom: None,
                    })
                    .collect(),
            );
        }
        if req.tool_choice.is_none()
            && let Some(fc) = req.function_call.clone()
        {
            req.tool_choice = Some(match fc {
                FunctionCallOption::Mode(mode) => ToolChoice::Mode(mode),
                FunctionCallOption::Named(named) => ToolChoice::Named(NamedToolChoice {
                    ty: "function".to_string(),
                    function: NamedFunction { name: named.name },
                }),
            });
        }

        let norm = normalize::apply(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let tool_ctx = tools::extract(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let prompt = prompt::build(&req, &tool_ctx);
        let model_res = resolver::resolve(
            &registry,
            &req.model,
            req.reasoning_effort.as_deref(),
            req.web_search_options.as_ref(),
            // 测试辅助：沿用与生产相同的默认值
            crate::config::DsCoreSection::default().default_search_enabled,
        )
        .map_err(OpenAIAdapterError::BadRequest)?;

        println!("\n=== PARSED REQUEST ===");
        println!("prompt:\n{prompt}");
        println!(
            "thinking={} search={}",
            model_res.thinking_enabled, model_res.search_enabled
        );
        println!("======================\n");

        Ok(TestRequest {
            prompt,
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: model_res.search_enabled,
            stream: req.stream,
            stop: norm.stop,
            opts: (norm.include_usage, norm.include_obfuscation),
        })
    }

    #[test]
    fn basic_chat() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "system", "content": "你是一个有帮助的助手。" },
                { "role": "user", "content": "你好" }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(!req.prompt.is_empty());
    }

    #[test]
    fn tool_conversation() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "北京天气怎么样？" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": { "name": "get_weather", "arguments": "{\"city\":\"北京\"}" }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc123",
                    "content": "北京今天晴，25°C。"
                },
                { "role": "user", "content": "谢谢" }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("get_weather"));
    }

    #[test]
    fn reasoning_and_search_flags() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "分析一下量子计算" }
            ],
            "reasoning_effort": "high",
            "web_search_options": { "search_context_size": "high" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.thinking_enabled);
        assert!(req.search_enabled);
    }

    // 裸 model_type 名（不带 deepseek- 前缀）也应可解析，见 issue #99
    #[test]
    fn bare_model_type_name_is_accepted() {
        for name in ["default", "DEFAULT", "deepseek-default"] {
            let body = serde_json::json!({
                "model": name,
                "messages": [{ "role": "user", "content": "hi" }]
            });
            let req = parse_json(body).unwrap_or_else(|e| panic!("模型 {name} 应被接受: {e}"));
            assert!(req.prompt.contains("hi"), "模型 {name} 应生成 prompt");
        }
    }

    // normalize 错误场景
    #[test]
    fn missing_model() {
        let body = serde_json::json!({
            "messages": [{ "role": "user", "content": "你好" }]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn missing_messages() {
        let body = serde_json::json!({
            "model": "deepseek-default"
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("messages"));
    }

    /// 规范里 n 必须 ≥ 1；上游只产出单候选，n > 1 必须显式报错而不是静默忽略
    #[test]
    fn n_out_of_range_is_rejected() {
        let mut body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "n": 2
        });
        let err = parse_json(body.clone()).unwrap_err();
        assert!(err.to_string().contains("n=2"), "实际: {err}");

        body["n"] = serde_json::json!(0);
        let err = parse_json(body.clone()).unwrap_err();
        assert!(err.to_string().contains("'n'"), "实际: {err}");

        body["n"] = serde_json::json!(1);
        assert!(parse_json(body).is_ok());
    }

    #[test]
    fn tool_missing_tool_call_id() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "tool", "content": "result" }
            ]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("tool_call_id"));
    }

    #[test]
    fn function_missing_name() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "function", "content": "result" }
            ]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("name"));
    }

    // model 解析错误与能力标志
    #[test]
    fn unsupported_model() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("不支持"));
    }

    #[test]
    fn reasoning_effort_variants() {
        for (effort, expected) in [
            ("minimal", true),
            ("low", true),
            ("medium", true),
            ("high", true),
            ("xhigh", true),
            ("unknown", true),
            ("", true),
        ] {
            let body = serde_json::json!({
                "model": "deepseek-default",
                "messages": [{ "role": "user", "content": "hi" }],
                "reasoning_effort": effort
            });
            let req = parse_json(body).unwrap();
            assert_eq!(req.thinking_enabled, expected, "reasoning_effort={effort}");
        }

        // 未提供 reasoning_effort 时默认开启 reasoning
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let req = parse_json(body).unwrap();
        assert!(
            req.thinking_enabled,
            "reasoning_effort absent should default to high"
        );
    }

    #[test]
    fn search_enabled_by_default() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let req = parse_json(body).unwrap();
        assert!(req.search_enabled);
    }

    // stop 序列与 stream_options 默认值

    #[test]
    fn stop_single() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "stop": "EOF"
        });
        let req = parse_json(body).unwrap();
        assert_eq!(req.stop, vec!["EOF"]);
    }

    #[test]
    fn stop_multiple() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "stop": ["STOP", "HALT"]
        });
        let req = parse_json(body).unwrap();
        assert_eq!(req.stop, vec!["STOP", "HALT"]);
    }

    #[test]
    fn stream_options() {
        // 默认值
        let req = parse_json(serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .unwrap();
        assert!(!req.stream);
        assert_eq!(req.opts, (false, true));

        // 显式覆盖
        let req2 = parse_json(serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream_options": { "include_usage": true, "include_obfuscation": false }
        }))
        .unwrap();
        assert_eq!(req2.opts, (true, false));
    }

    // tools 校验与注入

    #[test]
    fn tool_choice_none_ignores_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "function",
                    "function": { "name": "f", "parameters": {} }
                }
            ],
            "tool_choice": "none"
        });
        let req = parse_json(body).unwrap();
        assert!(!req.prompt.contains("你可以使用以下工具"));
    }

    #[test]
    fn tool_choice_required_instruction() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "function",
                    "function": { "name": "f" }
                }
            ],
            "tool_choice": "required"
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("注意：你必须调用一个或多个工具"));
    }

    #[test]
    fn parallel_tool_calls_false_instruction() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "f" } }
            ],
            "parallel_tool_calls": false
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("注意：一次只能调用一个工具"));
    }

    #[test]
    fn tool_choice_named_function() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "get_weather" } }
            ],
            "tool_choice": { "type": "function", "function": { "name": "get_weather" } }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("注意：你必须调用 'get_weather' 工具"));
    }

    #[test]
    fn tool_choice_allowed_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "get_weather" } },
                { "type": "function", "function": { "name": "get_time" } }
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "allowed_tools": {
                    "mode": "required",
                    "tools": [
                        { "type": "function", "function": { "name": "get_weather" } }
                    ]
                }
            }
        });
        let req = parse_json(body).unwrap();
        assert!(
            req.prompt
                .contains("你只能从以下允许的工具中选择：get_weather")
        );
        assert!(req.prompt.contains("注意：你必须调用一个或多个工具"));
    }

    #[test]
    fn tool_choice_custom() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "custom",
                    "custom": { "name": "my_custom", "format": { "type": "text" } }
                }
            ],
            "tool_choice": { "type": "custom", "custom": { "name": "my_custom" } }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("**my_custom** (custom):"));
        assert!(req.prompt.contains("你必须调用 'my_custom' 自定义工具"));
    }

    #[test]
    fn custom_tool_grammar_format() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "custom",
                    "custom": {
                        "name": "grammar_tool",
                        "description": " grammar based tool",
                        "format": {
                            "type": "grammar",
                            "grammar": {
                                "definition": "start: word+",
                                "syntax": "lark"
                            }
                        }
                    }
                }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("grammar(syntax: lark)"));
    }

    #[test]
    fn custom_tool_missing_format() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "custom",
                    "custom": { "name": "no_format" }
                }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("调用方法:"));
        assert!(req.prompt.contains("无约束"));
    }

    #[test]
    fn tool_empty_name() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "" } }
            ]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn tool_choice_required_without_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": "required"
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
    }

    #[test]
    fn allowed_tools_bad_mode() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "f" } }
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "allowed_tools": { "mode": "invalid", "tools": [] }
            }
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
    }

    // tools 注入位置：作为普通 System 内容注入一次，末尾保留 <｜Assistant｜>

    #[test]
    fn tools_injected_into_system_message_once() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "第一个问题" },
                { "role": "assistant", "content": "回答" },
                { "role": "user", "content": "第二个问题" }
            ],
            "tools": [
                { "type": "function", "function": { "name": "calc" } }
            ]
        });
        let req = parse_json(body).unwrap();
        let prompt = &req.prompt;

        // 工具定义位于 System 消息中
        let sys_pos = prompt.find("<｜System｜>").expect("应存在 System 段");
        let defs_pos = prompt.find("calc").expect("应包含工具定义");
        assert!(defs_pos > sys_pos, "工具定义应在 System 消息内");
        assert!(prompt.contains("工具调用格式"), "应包含调用格式规范");

        // 不得再使用未闭合 <think> 注入（规则文本里提到 <think> 不算）
        assert!(
            !prompt.contains("<｜Assistant｜><think>"),
            "不应再注入未闭合 <think> 块: {prompt}"
        );
        assert!(
            !prompt.contains("我刚刚被系统提醒"),
            "不应再注入角色扮演式元指令"
        );

        // 工具定义只注入一次（历史上 System 与 <think> 各注入一遍）
        assert_eq!(
            prompt.matches("你可以使用以下工具").count(),
            1,
            "工具定义只能出现一次"
        );
    }

    // 多轮历史（含 assistant 轮次）且无 tools 时，末尾仍必须有
    // <｜Assistant｜> 作为生成起点，否则 split_history_prompt 找不到拆分点，
    // 会把整段历史当成 inline prompt 直接发送。

    #[test]
    fn prompt_ends_with_assistant_anchor_for_multiturn_history() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "第一个问题" },
                { "role": "assistant", "content": "回答" },
                { "role": "user", "content": "第二个问题" }
            ]
        });
        let req = parse_json(body).unwrap();
        let prompt = &req.prompt;

        let last_assistant = prompt
            .rfind("<｜Assistant｜>")
            .expect("prompt 必须以 <｜Assistant｜> 结尾作为生成起点");
        let last_user_pos = prompt.rfind("第二个问题").unwrap();
        assert!(
            last_assistant > last_user_pos,
            "末尾 <｜Assistant｜> 应在最后一条 user 消息之后"
        );
        assert!(
            prompt[last_assistant + "<｜Assistant｜>".len()..]
                .trim()
                .is_empty(),
            "末尾 <｜Assistant｜> 之后不应再有内容: {:?}",
            &prompt[last_assistant..]
        );
    }

    // functions / function_call 兼容降级

    #[test]
    fn functions_legacy_to_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "北京天气？" }],
            "functions": [
                {
                    "name": "get_weather",
                    "description": "获取天气",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                }
            ],
            "function_call": "auto"
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("get_weather"));
        assert!(req.prompt.contains("你可以使用以下工具"));
    }

    #[test]
    fn function_call_named_legacy() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "查天气" }],
            "functions": [
                { "name": "get_weather", "parameters": {} }
            ],
            "function_call": { "name": "get_weather" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("你必须调用 'get_weather' 工具"));
    }

    #[test]
    fn tools_priority_over_functions() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "tool_a", "parameters": {} } }
            ],
            "functions": [
                { "name": "func_b", "parameters": {} }
            ],
            "tool_choice": "auto",
            "function_call": { "name": "func_b" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("tool_a"));
        assert!(!req.prompt.contains("func_b"));
    }

    // response_format 兼容降级

    #[test]
    fn response_format_json_object() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "输出 JSON" }],
            "response_format": { "type": "json_object" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("请直接输出合法的 JSON 对象"));
    }

    #[test]
    fn response_format_json_schema() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "结构化输出" }],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "person",
                    "schema": { "type": "object", "properties": { "name": { "type": "string" } } }
                }
            }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("遵守以下的格式"));
        assert!(req.prompt.contains("person"));
    }

    #[test]
    fn response_format_text_no_injection() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "text" }
        });
        let req = parse_json(body).unwrap();
        assert!(!req.prompt.contains("请以"));
        assert!(!req.prompt.contains("JSON"));
    }
}

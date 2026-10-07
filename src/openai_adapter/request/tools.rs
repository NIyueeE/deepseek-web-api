//! 工具解析 —— 校验 tools/tool_choice 并生成提示词注入文本
//!
//! 由于 ds_core 不支持原生 function calling，本模块将工具定义降级为
//! 自然语言描述，并追加到 prompt 中引导模型输出。

use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{
    AllowedTools, AllowedToolsChoice, ChatCompletionsRequest, CustomTool, CustomToolFormat,
    FunctionDefinition, Tool, ToolChoice,
};

/// 提取后的工具上下文
pub(crate) struct ToolContext {
    /// 格式模板 + 规则 + 示例（位于工具定义之前）
    pub format_block: Option<String>,
    /// 格式化后的工具定义文本
    pub defs_text: Option<String>,
    /// 根据 tool_choice / parallel_tool_calls 追加的行为指令
    pub instruction_text: Option<String>,
}

fn has_tools(req: &ChatCompletionsRequest) -> bool {
    req.tools.as_ref().is_some_and(|t| !t.is_empty())
}

/// 从请求中提取并校验工具信息
///
/// 当 tool_choice 为 none 时返回空的 ToolContext，不生成任何注入文本。
pub(crate) fn extract(req: &ChatCompletionsRequest) -> Result<ToolContext, String> {
    let default_choice = if has_tools(req) {
        ToolChoice::Mode("auto".to_string())
    } else {
        ToolChoice::Mode("none".to_string())
    };
    let tool_choice = req.tool_choice.as_ref().unwrap_or(&default_choice);

    validate_tool_choice(tool_choice, req.tools.as_deref())?;

    if matches!(tool_choice, ToolChoice::Mode(m) if m == "none") {
        return Ok(ToolContext {
            format_block: None,
            defs_text: None,
            instruction_text: None,
        });
    }

    let mut instruction_lines = Vec::new();

    match tool_choice {
        ToolChoice::Mode(mode) => {
            if mode == "required" {
                instruction_lines.push("**注意：你必须调用一个或多个工具。**".to_string());
            }
        }
        ToolChoice::AllowedTools(AllowedToolsChoice { allowed_tools, .. }) => {
            build_allowed_tools_instruction(allowed_tools, &mut instruction_lines);
        }
        ToolChoice::Named(named) => {
            instruction_lines.push(format!(
                "**注意：你必须调用 '{}' 工具。**",
                named.function.name
            ));
        }
        ToolChoice::Custom(custom) => {
            instruction_lines.push(format!(
                "**注意：你必须调用 '{}' 自定义工具。**",
                custom.custom.name
            ));
        }
    }

    if req.parallel_tool_calls == Some(false) {
        instruction_lines.push("**注意：一次只能调用一个工具。**".to_string());
    }

    let format_block = has_tools(req).then(|| build_tool_instruction_block(req));

    let defs_text = if has_tools(req) {
        let mut lines = vec!["你可以使用以下工具：".to_string()];
        for (i, tool) in req.tools.as_ref().unwrap().iter().enumerate() {
            lines.push(format_tool(tool, i)?);
        }
        Some(lines.join("\n"))
    } else {
        None
    };

    let instruction_text = if instruction_lines.is_empty() {
        None
    } else {
        Some(instruction_lines.join("\n"))
    };

    Ok(ToolContext {
        format_block,
        defs_text,
        instruction_text,
    })
}

fn validate_tool_choice(tc: &ToolChoice, tools: Option<&[Tool]>) -> Result<(), String> {
    match tc {
        ToolChoice::Mode(mode) => {
            if !matches!(mode.as_str(), "none" | "auto" | "required") {
                return Err(format!("tool_choice 无效模式: {mode}"));
            }
            if matches!(mode.as_str(), "auto" | "required") && tools.is_none_or(|t| t.is_empty()) {
                return Err("tool_choice 为 'auto' 或 'required' 时必须提供 tools".into());
            }
            Ok(())
        }
        ToolChoice::Named(_) | ToolChoice::Custom(_) => {
            if tools.is_none() {
                return Err("tool_choice 指定了具体工具时必须提供 tools".into());
            }
            Ok(())
        }
        ToolChoice::AllowedTools(AllowedToolsChoice { allowed_tools, .. }) => {
            if tools.is_none() {
                return Err("tool_choice 指定了 allowed_tools 时必须提供 tools".into());
            }
            if !matches!(allowed_tools.mode.as_str(), "auto" | "required") {
                return Err(format!(
                    "allowed_tools.mode 必须是 'auto' 或 'required'，收到: {}",
                    allowed_tools.mode
                ));
            }
            Ok(())
        }
    }
}

fn build_allowed_tools_instruction(allowed_tools: &AllowedTools, lines: &mut Vec<String>) {
    if let Some(tool_list) = &allowed_tools.tools {
        let names: Vec<String> = tool_list
            .iter()
            .filter_map(|v| v.get("function").and_then(|f| f.get("name")))
            .filter_map(|n| n.as_str().map(|s| s.to_string()))
            .collect();
        if !names.is_empty() {
            lines.push(format!(
                "**注意：**你只能从以下允许的工具中选择：{}。",
                names.join(", ")
            ));
        }
    }

    if allowed_tools.mode == "required" {
        lines.push("**注意：你必须调用一个或多个工具。**".to_string());
    }
}

fn format_tool(tool: &Tool, idx: usize) -> Result<String, String> {
    match tool.ty.as_str() {
        "function" => {
            let func = tool.function.as_ref().ok_or_else(|| {
                format!("tools[{idx}] 类型为 'function' 时必须提供 function 定义")
            })?;
            format_function(func)
        }
        "custom" => {
            let custom = tool
                .custom
                .as_ref()
                .ok_or_else(|| format!("tools[{idx}] 类型为 'custom' 时必须提供 custom 定义"))?;
            Ok(format_custom(custom))
        }
        _ => Err(format!("tools[{}] 不支持的类型: {}", idx, tool.ty)),
    }
}

fn format_function(func: &FunctionDefinition) -> Result<String, String> {
    if func.name.trim().is_empty() {
        return Err("tools 中 function 缺少必填字段 'name'".into());
    }
    let params = serde_json::to_string(&func.parameters).unwrap_or_else(|_| "{}".into());
    let call_example = format!(
        "{TOOL_CALL_START}[{{\"name\": \"{}\", \"arguments\": {}}}]{TOOL_CALL_END}",
        func.name, params
    );
    let desc = func.description.as_deref().unwrap_or("").trim();
    let desc_block = if desc.is_empty() {
        "  无描述".to_string()
    } else {
        format!("~~~markdown\n  {desc}\n~~~\n")
    };
    Ok(format!(
        "- **{}** (function):\n  - 调用方法: `{}`\n  - 简要说明:\n{}",
        func.name, call_example, desc_block,
    ))
}

/// 构建工具调用指令块：模板 → 规则 → 动态正确示例
fn build_tool_instruction_block(req: &ChatCompletionsRequest) -> String {
    let mut lines: Vec<String> = Vec::new();

    // 模板
    lines.push("**工具调用格式 — 请严格遵守：**".into());
    lines.push(String::new());
    lines.push("将 JSON 数组包裹在工具调用标记中：".into());
    lines.push(String::new());
    lines.push(format!(
        "{TOOL_CALL_START}[{{\"name\": \"工具名\", \"arguments\": {{参数JSON}}}}]{TOOL_CALL_END}"
    ));
    lines.push(String::new());

    // 规则
    lines.push("**规则：**".into());
    lines.push(String::new());
    lines.push(
        "**核心：决定调用工具时，你的响应中只允许出现工具调用文本本身，禁止任何解释、前缀、总结、问候语等额外内容。**".into(),
    );
    lines.push(String::new());
    lines.push(format!("1. JSON 数组必须以 `{TOOL_CALL_START}` 开头、以 `{TOOL_CALL_END}` 结尾，将数组**完整包裹**在标记内。"));
    lines.push("2. 所有工具调用必须放在**一个** JSON 数组中，多个调用用逗号分隔。".into());
    lines.push(format!(
        "3. 输出 `{TOOL_CALL_END}` 后**立即停止**，不得添加后续文本、XML 标签或说明文字。"
    ));
    lines.push("4. 不要将工具调用包裹在 markdown 代码块中。".into());
    lines.push("5. 字符串参数值必须用**双引号**包裹（JSON 标准）。".into());
    lines.push(format!(
        "6. 决定调用工具时，输出的**第一个非空白字符**必须是 `{TOOL_CALL_START}`。"
    ));
    lines.push(format!(
        "7. 整个响应中**只能出现一个 `{TOOL_CALL_START}` 块**，不要重复输出多个 `{TOOL_CALL_START}` 块。"
    ));
    lines.push(format!(
        "8. **重复：** 整个响应中只能出现一个 `{TOOL_CALL_START}` 块，不要重复输出。如果你已经输出了一个 `{TOOL_CALL_START}` 块，绝对不要再输出第二个。"
    ));
    lines.push(format!(
        "9. **重复：** 禁止在 `{TOOL_CALL_START}` 之前输出任何文字，包括但不限于解释、确认、总结、问候语。"
    ));
    lines.push("10. 不要把回复和工具调用置于思考内容中。".to_string());
    lines.push(
        "11. **重复：** 思考内容（<think> 标签内）仅用于内部推理过程，不要将最终回复或工具调用放在 <think> 标签中。".to_string(),
    );
    lines.push(String::new());

    let tool_names: Vec<String> = req
        .tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|t| t.function.as_ref().map(|f| f.name.clone()))
        .collect();
    let a = tool_names.first().map_or("tool_a", |s| s.as_str());

    // 正确示例（使用实际工具名，带真实参数）
    lines.push("**正确示例：**".into());
    lines.push(String::new());

    // 示例A：单个工具
    lines.push("**示例A** — 调用一个工具：".into());
    lines.push(format!(
        "{TOOL_CALL_START}[{{\"name\": \"{a}\", \"arguments\": {}}}]{TOOL_CALL_END}",
        example_args(a)
    ));
    lines.push(String::new());

    // 示例B：两个工具并行
    if tool_names.len() >= 2 {
        let items: Vec<String> = tool_names[..2]
            .iter()
            .map(|n| format!("{{\"name\": \"{n}\", \"arguments\": {}}}", example_args(n)))
            .collect();
        lines.push("**示例B** — 同时调用多个工具（一个数组包含全部调用）：".into());
        lines.push(String::new());
        lines.push(format!(
            "{TOOL_CALL_START}[{}]{TOOL_CALL_END}",
            items.join(", ")
        ));
        lines.push(String::new());
    }

    // 示例C：三个工具并行
    if tool_names.len() >= 3 {
        let items: Vec<String> = tool_names[..3]
            .iter()
            .map(|n| format!("{{\"name\": \"{n}\", \"arguments\": {}}}", example_args(n)))
            .collect();
        lines.push("**示例C** — 同时调用三个工具（所有调用在一个数组中）：".into());
        lines.push(String::new());
        lines.push(format!(
            "{TOOL_CALL_START}[{}]{TOOL_CALL_END}",
            items.join(", ")
        ));
        lines.push(String::new());
    }

    // 示例D：嵌套参数（参数值为数组或对象时仍是标准 JSON）
    if !tool_names.is_empty() {
        let d_name = tool_names.first().map_or("tool_a", |s| s.as_str());
        lines.push("**示例D** — 参数值为嵌套对象/数组（仍然是标准 JSON）：".into());
        lines.push(String::new());
        lines.push(format!(
            "{TOOL_CALL_START}[{{\"name\": \"{d_name}\", \"arguments\": {}}}]{TOOL_CALL_END}",
            example_nested_args(d_name)
        ));
        lines.push(String::new());
    }

    lines.join("\n")
}

/// 根据工具名返回示例参数字符串
fn example_args(name: &str) -> String {
    let args: &str = match name {
        "Read" | "read_file" => r#""file_path": "/path/to/file""#,
        "Bash" | "execute_command" | "exec_command" => r#""command": "ls -la""#,
        "Write" | "write_to_file" => r#""file_path": "/path/to/file", "content": "hello""#,
        "Edit" => r#""file_path": "/path/to/file", "old_string": "foo", "new_string": "bar""#,
        "Glob" => r#""pattern": "**/*.rs", "path": "."#,
        "search_files" => r#""query": "TODO", "path": "."#,
        "get_weather" => r#""city": "Beijing""#,
        "get_time" => r#""timezone": "Asia/Shanghai""#,
        "list_files" => r#""path": "."#,
        _ => r#""key": "value""#,
    };
    format!("{{{args}}}")
}

/// 返回嵌套参数示例（参数值为数组或对象）
fn example_nested_args(name: &str) -> String {
    match name {
        "Edit" => r#"{"file_path": "/path/to/file", "edits": [{"old_string": "foo", "new_string": "bar"}, {"old_string": "x", "new_string": "y"}]}"#.into(),
        _ => r#"{"config": {"enabled": true, "items": ["a", "b"]}}"#.into(),
    }
}

fn format_custom(custom: &CustomTool) -> String {
    let desc = custom.description.as_deref().unwrap_or("").trim();
    let method = match &custom.format {
        Some(CustomToolFormat::Text) => "text".into(),
        Some(CustomToolFormat::Grammar { grammar }) => {
            format!("grammar(syntax: {})", grammar.syntax)
        }
        None => "无约束".into(),
    };
    format!(
        "- **{}** (custom):\n  - 调用方法: `{}`\n  - 简要说明: {}",
        custom.name,
        method,
        if desc.is_empty() { "无描述" } else { desc },
    )
}

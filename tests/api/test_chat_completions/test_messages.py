"""Unit tests for messages.py - message conversion utilities."""



from deepseek_web_api.api.openai.chat_completions.messages import (
    convert_messages_to_prompt,
    extract_text_content,
)


class TestExtractTextContent:
    """Tests for extract_text_content function."""

    def test_none_returns_empty(self):
        assert extract_text_content(None) == ""

    def test_string_returns_unchanged(self):
        assert extract_text_content("hello world") == "hello world"

    def test_list_with_text_block(self):
        content = [{"type": "text", "text": "hello"}]
        assert extract_text_content(content) == "hello"

    def test_list_with_multiple_text_blocks(self):
        content = [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}]
        assert extract_text_content(content) == "hello\n\nworld"

    def test_list_ignores_non_text_blocks(self):
        content = [{"type": "image", "text": "hello"}]
        assert extract_text_content(content) == ""

    def test_list_with_object_having_text_attr(self):
        class TextBlock:
            text = "hello"

        assert extract_text_content([TextBlock()]) == "hello"

    def test_empty_list(self):
        assert extract_text_content([]) == ""


class TestConvertMessagesToPrompt:
    """Tests for convert_messages_to_prompt function."""

    def test_empty_messages(self):
        result = convert_messages_to_prompt([])
        assert result == ""

    def test_user_message(self):
        messages = [{"role": "user", "content": "Hello"}]
        result = convert_messages_to_prompt(messages)
        assert "User: Hello" in result

    def test_system_message(self):
        messages = [{"role": "system", "content": "You are helpful"}]
        result = convert_messages_to_prompt(messages)
        assert "[System Instruction]" in result
        assert "You are helpful" in result

    def test_assistant_message(self):
        messages = [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi there"},
        ]
        result = convert_messages_to_prompt(messages)
        assert "Assistant: Hi there" in result

    def test_assistant_with_tool_calls(self):
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": {"city": "Beijing"}
                        }
                    }
                ]
            }
        ]
        result = convert_messages_to_prompt(messages)
        assert "[TOOL🛠️]" in result
        assert "get_weather" in result
        assert "[/TOOL🛠️]" in result

    def test_tool_message(self):
        messages = [
            {"role": "user", "content": "What's the weather?"},
            {
                "role": "assistant",
                "content": "Let me check...",
                "tool_calls": [
                    {
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": {}}
                    }
                ]
            },
            {"role": "tool", "tool_call_id": "call_123", "content": "Sunny, 25°C"}
        ]
        result = convert_messages_to_prompt(messages)
        assert "Tool: id=call_123" in result
        assert "Sunny, 25°C" in result

    def test_system_instruction_wraps_user_and_assistant(self):
        messages = [
            {"role": "system", "content": "You are a chatbot."},
            {"role": "user", "content": "Hi"},
        ]
        result = convert_messages_to_prompt(messages)
        assert result.startswith("[System Instruction]")
        assert "User: Hi" in result

    def test_tools_injected_into_system_instruction(self):
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {
                                "type": "string",
                                "description": "City name"
                            }
                        },
                        "required": ["city"]
                    }
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert "## Available Tools" in result
        assert "get_weather" in result
        assert "city" in result
        assert "[TOOL🛠️]" in result
        assert "[/TOOL🛠️]" in result

    def test_tool_call_with_string_arguments(self):
        """Test that string arguments are parsed as JSON."""
        messages = [
            {
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": '{"query": "weather"}'
                        }
                    }
                ]
            }
        ]
        result = convert_messages_to_prompt(messages)
        # Arguments string should be parsed and included
        assert "search" in result

    def test_tool_reminder_added_when_tools_present(self):
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "test",
                    "description": "A test tool",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Hi"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert "[REMINDER]" in result
        assert "[TOOL🛠️]" in result


class TestToolChoiceAndStrict:
    """Tests for tool_choice, parallel_tool_calls, strict mode, and JSON Schema features."""

    def test_tool_choice_auto_injects_tools(self):
        """tool_choice='auto' with tools → tools appear, normal reminder."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools, tool_choice="auto")
        assert "get_weather" in result
        assert "[TOOL🛠️]" in result
        assert "[REMINDER]" in result

    def test_tool_choice_none_reminder_forbids_tools(self):
        """tool_choice='none' → tools DO NOT appear, NO REMINDER about tools."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools, tool_choice="none")
        assert "get_weather" not in result
        assert "## Available Tools" not in result
        assert "[REMINDER]" not in result

    def test_tool_choice_required_reminder_enforces_tool(self):
        """tool_choice='required' + tools exist → MUST call at least one tool reminder."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools, tool_choice="required")
        assert "get_weather" in result
        assert "[REMINDER]" in result
        assert "MUST call at least one tool" in result

    def test_tool_choice_required_no_tools_degrades(self):
        """tool_choice='required' + empty tools list → degraded reminder, tools absent."""
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=[], tool_choice="required")
        assert "## Available Tools" not in result
        assert "[REMINDER]" in result
        assert "no tools are available" in result

    def test_tool_choice_specific_name_filters_tools(self):
        """tool_choice with specific name that exists → only that tool appears."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "get_time",
                    "description": "Get time",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(
            messages,
            tools=tools,
            tool_choice={"type": "function", "function": {"name": "get_weather"}}
        )
        assert "get_weather" in result
        assert "get_time" not in result

    def test_tool_choice_specific_name_not_found_degrades(self):
        """tool_choice with nonexistent name + tools exist → degraded reminder."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(
            messages,
            tools=tools,
            tool_choice={"type": "function", "function": {"name": "nonexistent"}}
        )
        assert "## Available Tools" not in result
        assert "[REMINDER]" in result
        assert "nonexistent" in result
        assert "not available" in result

    def test_parallel_tool_calls_false_reminder_at_most_one(self):
        """parallel_tool_calls=False → 'ONE tool' (word boundary) appears in reminder."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools, parallel_tool_calls=False)
        # Use word-boundary-aware check: "ONE tool" not "nonexistent" substring
        assert "ONE tool" in result or "Call only ONE" in result
        assert "[REMINDER]" in result

    def test_parallel_tool_calls_false_with_degraded_skips_constraint(self):
        """parallel_tool_calls=False + degraded (tool_choice=nonexistent) → NO 'ONE tool' constraint."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(
            messages,
            tools=tools,
            tool_choice={"type": "function", "function": {"name": "nonexistent"}},
            parallel_tool_calls=False
        )
        # Should have degraded reminder but NOT the ONE tool constraint
        assert "[REMINDER]" in result
        assert "not available" in result
        # The key: "ONE tool" should NOT appear when degraded
        assert "ONE tool" not in result
        assert "Call only ONE" not in result

    def test_strict_true_injects_mode_notice(self):
        """Tool with strict=True → 'Strict Mode' paragraph appears."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}},
                    "strict": True
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert "Strict Mode" in result

    def test_strict_false_no_mode_notice(self):
        """Tool with strict=False or omitted → no 'Strict Mode' paragraph."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}},
                    "strict": False
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert "Strict Mode" not in result

    def test_mixed_strict_tools(self):
        """One tool strict:true, one strict:false → 'Strict Mode' appears exactly once."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}},
                    "strict": True
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "get_time",
                    "description": "Get time",
                    "parameters": {"type": "object", "properties": {}},
                    "strict": False
                }
            }
        ]
        messages = [{"role": "user", "content": "Weather?"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert result.count("Strict Mode") == 1

    def test_complex_nested_schema_with_strict(self):
        """Deeply nested schema + strict:true → JSON Schema code block contains nested structure."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_data",
                    "description": "Get nested data",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "config": {
                                "type": "object",
                                "properties": {
                                    "settings": {
                                        "type": "object",
                                        "properties": {
                                            "timeout": {"type": "integer"}
                                        }
                                    }
                                }
                            }
                        },
                        "required": ["config"]
                    },
                    "strict": True
                }
            }
        ]
        messages = [{"role": "user", "content": "Get data?"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert "Strict Mode" in result
        assert "```json" in result
        assert "config" in result
        assert "settings" in result
        assert "timeout" in result

    def test_json_schema_block_and_natural_language_both_present(self):
        """Tool with enum parameter → both JSON code block AND natural language appear."""
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "set_status",
                    "description": "Set status to one of the allowed values",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "status": {
                                "type": "string",
                                "enum": ["active", "inactive", "pending"],
                                "description": "The status value"
                            }
                        },
                        "required": ["status"]
                    }
                }
            }
        ]
        messages = [{"role": "user", "content": "Set status?"}]
        result = convert_messages_to_prompt(messages, tools=tools)
        assert "```json" in result
        assert "enum" in result
        assert "active" in result
        assert "status" in result
        assert "allowed values" in result or "The status value" in result

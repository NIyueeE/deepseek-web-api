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

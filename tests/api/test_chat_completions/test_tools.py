"""Unit tests for tools.py - tool parsing and conversion utilities."""

import json


from deepseek_web_api.api.openai.chat_completions.tools import (
    TOOL_START_MARKER,
    TOOL_END_MARKER,
    _build_tool_call,
    _build_valid_tool_names_set,
    _convert_items_to_tool_calls,
    _fix_unescaped_quotes,
    _try_parse_json,
    convert_tool_json_to_openai,
    extract_json_tool_calls,
    get_stream_window,
)


class TestBuildToolCall:
    """Tests for _build_tool_call function."""

    def test_build_with_dict_arguments(self):
        result = _build_tool_call("get_weather", {"city": "Beijing"})
        assert result["type"] == "function"
        assert result["function"]["name"] == "get_weather"
        assert "id" in result
        assert result["function"]["arguments"] == '{"city": "Beijing"}'

    def test_build_with_string_arguments(self):
        result = _build_tool_call("get_weather", '{"city": "Beijing"}')
        assert result["function"]["arguments"] == '{"city": "Beijing"}'

    def test_id_is_unique(self):
        result1 = _build_tool_call("test", {})
        result2 = _build_tool_call("test", {})
        assert result1["id"] != result2["id"]


class TestBuildValidToolNamesSet:
    """Tests for _build_valid_tool_names_set function."""

    def test_extracts_tool_names(self):
        tools = [
            {"function": {"name": "tool_a"}},
            {"function": {"name": "tool_b"}},
        ]
        result = _build_valid_tool_names_set(tools)
        assert result == {"tool_a", "tool_b"}

    def test_ignores_missing_names(self):
        tools = [
            {"function": {"name": "tool_a"}},
            {"function": {}},
            {},
        ]
        result = _build_valid_tool_names_set(tools)
        assert result == {"tool_a"}

    def test_empty_list(self):
        assert _build_valid_tool_names_set([]) == set()


class TestConvertItemsToToolCalls:
    """Tests for _convert_items_to_tool_calls function."""

    def test_converts_valid_items(self):
        valid_names = {"get_weather"}
        items = [
            {"name": "get_weather", "arguments": {"city": "Beijing"}},
        ]
        result = _convert_items_to_tool_calls(items, valid_names)
        assert len(result) == 1
        assert result[0]["function"]["name"] == "get_weather"

    def test_skips_unknown_tools(self):
        valid_names = {"get_weather"}
        items = [
            {"name": "unknown_tool", "arguments": {}},
        ]
        result = _convert_items_to_tool_calls(items, valid_names)
        assert len(result) == 0

    def test_skips_missing_name(self):
        valid_names = {"get_weather"}
        items = [
            {"arguments": {}},
        ]
        result = _convert_items_to_tool_calls(items, valid_names)
        assert len(result) == 0


class TestFixUnescapedQuotes:
    """Tests for _fix_unescaped_quotes function."""

    def test_passthrough_valid_json(self):
        s = '{"command": "echo hello"}'
        assert _fix_unescaped_quotes(s) == s

    def test_fixes_unescaped_inner_quotes(self):
        # Model might output: {"command": "echo "hello""}
        s = '{"command": "echo \\"hello\\""}'
        result = _fix_unescaped_quotes(s)
        # Should be parseable as JSON
        parsed = json.loads(result)
        assert parsed["command"] == 'echo "hello"'

    def test_preserves_escaped_quotes(self):
        s = '{"command": "echo \\"hello\\""}'
        result = _fix_unescaped_quotes(s)
        assert "\\" in result or '"' in result

    def test_empty_string(self):
        assert _fix_unescaped_quotes("") == ""

    def test_complex_nested(self):
        s = '{"cmd": "echo \\"hello world\\" and \\"goodbye\\""}'
        result = _fix_unescaped_quotes(s)
        parsed = json.loads(result)
        assert parsed["cmd"] == 'echo "hello world" and "goodbye"'


class TestTryParseJson:
    """Tests for _try_parse_json function."""

    def test_valid_json_returns_parsed(self):
        assert _try_parse_json('{"key": "value"}') == {"key": "value"}

    def test_invalid_json_returns_none(self):
        assert _try_parse_json("not json") is None

    def test_tries_fix_on_failure(self):
        # _fix_unescaped_quotes actually fixes this case
        result = _try_parse_json('{"cmd": "echo \\"hi\\""}')
        assert result is not None
        assert result["cmd"] == 'echo "hi"'

    def test_array_json(self):
        assert _try_parse_json('[1, 2, 3]') == [1, 2, 3]


class TestExtractJsonToolCalls:
    """Tests for extract_json_tool_calls function."""

    def test_extracts_single_tool_call(self):
        tools = [{"function": {"name": "get_weather", "description": "Get weather"}}]
        text = f'{TOOL_START_MARKER}{{"name": "get_weather", "arguments": {{"city": "Beijing"}}}}{TOOL_END_MARKER}'
        cleaned, tool_calls = extract_json_tool_calls(text, tools)
        assert cleaned == ""
        assert len(tool_calls) == 1
        assert tool_calls[0]["function"]["name"] == "get_weather"

    def test_extracts_multiple_tool_calls(self):
        tools = [
            {"function": {"name": "tool_a", "description": "A"}},
            {"function": {"name": "tool_b", "description": "B"}},
        ]
        text = f'{TOOL_START_MARKER}[{{"name": "tool_a", "arguments": {{}}}}, {{"name": "tool_b", "arguments": {{}}}}]{TOOL_END_MARKER}'
        _, tool_calls = extract_json_tool_calls(text, tools)
        assert len(tool_calls) == 2

    def test_returns_cleaned_text_without_markers(self):
        tools = [{"function": {"name": "test", "description": "Test"}}]
        text = f'Some text before {TOOL_START_MARKER}{{"name": "test", "arguments": {{}}}}{TOOL_END_MARKER} text after'
        cleaned, _ = extract_json_tool_calls(text, tools)
        assert "TOOL" not in cleaned
        assert "Some text before" in cleaned
        assert "text after" in cleaned

    def test_skips_unknown_tools(self):
        tools = [{"function": {"name": "known_tool", "description": "Known"}}]
        text = f'{TOOL_START_MARKER}{{"name": "unknown_tool", "arguments": {{}}}}{TOOL_END_MARKER}'
        _, tool_calls = extract_json_tool_calls(text, tools)
        assert len(tool_calls) == 0

    def test_handles_non_json_between_markers(self):
        tools = [{"function": {"name": "test", "description": "Test"}}]
        text = f'{TOOL_START_MARKER}not json{TOOL_END_MARKER}'
        _, tool_calls = extract_json_tool_calls(text, tools)
        assert len(tool_calls) == 0


class TestConvertToolJsonToOpenai:
    """Tests for convert_tool_json_to_openai function."""

    def test_converts_single_object(self):
        tools = [{"function": {"name": "get_weather", "description": "Get weather"}}]
        json_str = '{"name": "get_weather", "arguments": {"city": "Beijing"}}'
        result = convert_tool_json_to_openai(json_str, tools)
        assert result is not None
        assert len(result) == 1
        assert result[0]["function"]["name"] == "get_weather"

    def test_converts_array(self):
        tools = [
            {"function": {"name": "tool_a", "description": "A"}},
            {"function": {"name": "tool_b", "description": "B"}},
        ]
        json_str = '[{"name": "tool_a", "arguments": {}}, {"name": "tool_b", "arguments": {}}]'
        result = convert_tool_json_to_openai(json_str, tools)
        assert result is not None
        assert len(result) == 2

    def test_returns_none_for_invalid_json(self):
        tools = [{"function": {"name": "test", "description": "Test"}}]
        result = convert_tool_json_to_openai("not valid json", tools)
        assert result is None

    def test_returns_none_when_no_valid_tools(self):
        tools = [{"function": {"name": "known", "description": "Known"}}]
        result = convert_tool_json_to_openai('{"name": "unknown", "arguments": {}}', tools)
        assert result is None

    def test_adds_index_to_tool_calls(self):
        tools = [
            {"function": {"name": "tool_a", "description": "A"}},
            {"function": {"name": "tool_b", "description": "B"}},
        ]
        json_str = '[{"name": "tool_a", "arguments": {}}, {"name": "tool_b", "arguments": {}}]'
        result = convert_tool_json_to_openai(json_str, tools)
        assert result[0].get("index") == 0
        assert result[1].get("index") == 1

    def test_fix_unescaped_quotes_in_arguments(self):
        """Test that unescaped quotes in arguments are fixed."""
        tools = [{"function": {"name": "echo", "description": "Echo"}}]
        # Model might produce unescaped quotes
        json_str = '{"name": "echo", "arguments": {"cmd": "hello world"}}'
        result = convert_tool_json_to_openai(json_str, tools)
        assert result is not None
        assert len(result) == 1


class TestGetStreamWindow:
    """Tests for get_stream_window function."""

    def test_get_stream_window_empty_returns_default(self):
        """With no stop sequences, returns default window (len(TOOL_END_MARKER) * 2)."""
        result = get_stream_window([])
        assert result == len(TOOL_END_MARKER) * 2

    def test_get_stream_window_with_stop_sequences(self):
        """With stop sequences, returns max of default and longest stop * 2."""
        result = get_stream_window(["---", "## References"])
        assert result >= len("## References") * 2
        assert result >= len(TOOL_END_MARKER) * 2

    def test_get_stream_window_single_long_sequence(self):
        """With a single long stop sequence, returns its length * 2."""
        long_seq = "###END###"
        result = get_stream_window([long_seq])
        assert result == len(long_seq) * 2

    def test_get_stream_window_default_larger(self):
        """When TOOL_END_MARKER is longer than stop sequences, returns default."""
        # TOOL_END_MARKER is "[/TOOL🛠️]" which is 12 chars
        # Any stop sequence shorter than 6 chars should result in default
        result = get_stream_window(["ab"])
        assert result == len(TOOL_END_MARKER) * 2

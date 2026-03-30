"""Tool parsing and conversion utilities for OpenAI function calling."""

import json
import re
import uuid
from typing import List, Union

from ....core.logger import logger


TOOL_START_MARKER = "[TOOL🛠️]"
TOOL_END_MARKER = "[/TOOL🛠️]"
TOOL_JSON_PATTERN = re.compile(r'\[TOOL🛠️\](.*?)\[/TOOL🛠️\]', re.DOTALL)
# Sliding window for tool buffer: end marker length + 3 chars lookahead
STREAM_WINDOW = len(TOOL_END_MARKER) * 2

# Backward compatibility alias
TOOL_BUFFER_WINDOW = STREAM_WINDOW


def get_stream_window(stop_sequences: list[str]) -> int:
    """Compute dynamic buffer window based on stop sequences.
    
    Formula: max(len(TOOL_END_MARKER) * 2, max(len(s) for s in stop_sequences) * 2)
            if stop_sequences else len(TOOL_END_MARKER) * 2
    """
    if not stop_sequences:
        return len(TOOL_END_MARKER) * 2
    return max(len(TOOL_END_MARKER) * 2, max(len(s) for s in stop_sequences) * 2)


def _build_tool_call(tool_name: str, arguments: Union[str, dict]) -> dict:
    """Build a standard OpenAI tool_call dict."""
    return {
        "id": f"call_{uuid.uuid4().hex[:24]}",
        "type": "function",
        "function": {
            "name": tool_name,
            "arguments": arguments if isinstance(arguments, str) else json.dumps(arguments, ensure_ascii=False)
        }
    }


def _build_valid_tool_names_set(available_tools: list) -> set:
    """Build a set of valid tool names for O(1) lookup."""
    return {t.get("function", {}).get("name") for t in available_tools if t.get("function", {}).get("name")}


def _convert_items_to_tool_calls(items: list, valid_tool_names: set) -> list:
    """Convert parsed JSON items to OpenAI tool_calls format. Returns empty list if no valid calls."""
    tool_calls = []
    for item in items:
        tool_name = item.get("name")
        arguments = item.get("arguments", {})
        if not tool_name or tool_name not in valid_tool_names:
            continue
        tool_calls.append(_build_tool_call(tool_name, arguments))
    return tool_calls


def extract_json_tool_calls(text: str, available_tools: List[dict]):
    """Extract and validate JSON tool calls from response text.

    Model returns: [{"name": "func_name", "arguments": {...}}, ...] or {"name": "func_name", "arguments": {...}}
    Service adds: index, id, type
    """
    tool_calls = []

    valid_names = _build_valid_tool_names_set(available_tools)
    for match in TOOL_JSON_PATTERN.finditer(text):
        try:
            obj = json.loads(match.group(1))
            items = obj if isinstance(obj, list) else [obj]
            for item in items:
                tool_name = item.get("name")
                arguments = item.get("arguments", {})
                if not tool_name or tool_name not in valid_names:
                    logger.warning(f"Unknown tool: {tool_name}")
                    continue
                tc = _build_tool_call(tool_name, arguments)
                tc["index"] = len(tool_calls)
                tool_calls.append(tc)
        except json.JSONDecodeError:
            continue

    cleaned_text = TOOL_JSON_PATTERN.sub('', text)
    return cleaned_text.strip(), tool_calls


def _try_parse_json(json_str: str):
    """Try to parse JSON, return parsed object or None. Attempts basic fixes on failure."""
    # First try standard parse
    try:
        return json.loads(json_str)
    except json.JSONDecodeError:
        pass

    # Try to fix unescaped quotes inside string values
    # Pattern: within a string value, fix quotes that aren't properly escaped
    # This handles cases like: "command": "echo "text" more" where inner quotes should be \"
    try:
        fixed = _fix_unescaped_quotes(json_str)
        return json.loads(fixed)
    except (json.JSONDecodeError, RecursionError):
        pass

    return None


def _fix_unescaped_quotes(s: str) -> str:
    """Fix unescaped quotes in JSON string values.

    Handles the common model mistake of not escaping quotes inside strings,
    e.g., converts: {"command": "echo "hi"} to {"command": "echo \"hi"}
    """
    result = []
    i = 0
    while i < len(s):
        c = s[i]
        if c == '"':
            # Start of string - find the end, handling escaped quotes
            result.append(c)
            i += 1
            while i < len(s):
                c = s[i]
                if c == '\\':
                    # Escaped character - keep as is
                    result.append(c)
                    i += 1
                    if i < len(s):
                        result.append(s[i])
                        i += 1
                elif c == '"':
                    # End of string
                    result.append(c)
                    i += 1
                    break
                else:
                    result.append(c)
                    i += 1
        else:
            result.append(c)
            i += 1
    return ''.join(result)


def convert_tool_json_to_openai(json_str: str, available_tools: List[dict]):
    """Convert tool JSON from model format to OpenAI tool_calls format.

    Handles both single object: {"name": "func", "arguments": {...}}
    and array: [{"name": "func1", "arguments": {...}}, {"name": "func2", "arguments": {...}}]
    """
    obj = _try_parse_json(json_str)
    if obj is None:
        logger.warning(f"Failed to parse tool JSON: {json_str[:200]}...")
        return None

    valid_names = _build_valid_tool_names_set(available_tools)
    items = obj if isinstance(obj, list) else [obj]
    tool_calls = []
    for item in items:
        tool_name = item.get("name")
        arguments = item.get("arguments", {})
        if not tool_name or tool_name not in valid_names:
            continue
        tc = _build_tool_call(tool_name, arguments)
        tc["index"] = len(tool_calls)
        tool_calls.append(tc)
    return tool_calls if tool_calls else None

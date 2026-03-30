"""Message conversion utilities for OpenAI-style messages to DeepSeek prompt."""

import json
import logging
from typing import List, Optional, Union

from .tools import TOOL_START_MARKER, TOOL_END_MARKER

logger = logging.getLogger(__name__)


def _preprocess_tools(tools: Optional[List[dict]], tool_choice: Union[str, dict]) -> tuple[Optional[List[dict]], dict]:
    """Preprocess tools based on tool_choice.

    Returns: (effective_tools, tool_choice_info)
    - effective_tools: filtered list, or None if tools should not be exposed
    - tool_choice_info: dict with keys:
        - degraded: bool
        - reason: str (None, "no_tools_available", "tool_not_found", "invalid_value")
        - missing_name: str (only if reason == "tool_not_found")
    """
    info: dict = {"degraded": False, "reason": None, "missing_name": None}

    # Handle "none" - tools explicitly disabled
    if tool_choice == "none":
        return None, info

    # Handle "auto" or "required" - use all tools as-is
    if tool_choice == "auto":
        return tools, info

    if tool_choice == "required":
        if not tools:
            info["degraded"] = True
            info["reason"] = "no_tools_available"
        return tools if tools else None, info

    # Handle specific tool selection via dict
    if isinstance(tool_choice, dict):
        func_spec = tool_choice.get("function")
        if func_spec and isinstance(func_spec, dict):
            name = func_spec.get("name")
            if name and tools:
                for t in tools:
                    if t.get("function", {}).get("name") == name:
                        return [t], info
                # Tool not found
                info["degraded"] = True
                info["reason"] = "tool_not_found"
                info["missing_name"] = name
                return None, info

    # Invalid tool_choice value - degrade but preserve original tools
    info["degraded"] = True
    info["reason"] = "invalid_value"
    return tools, info


def extract_text_content(content: Union[str, List, None]) -> str:
    """Extract plain text from OpenAI message content field.

    Handles both string content and list content blocks (e.g., [{"type": "text", "text": "..."}]).
    """
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        texts = []
        for block in content:
            if isinstance(block, dict):
                if block.get('type') == 'text':
                    texts.append(block.get('text', ''))
            elif hasattr(block, 'text') and block.text:
                texts.append(block.text)
        return '\n\n'.join(texts)
    return ""


def convert_messages_to_prompt(
    messages: List[dict],
    tools: Optional[List[dict]] = None,
    tool_choice: Union[str, dict] = "auto",
    parallel_tool_calls: bool = True,
    response_format: Optional[dict] = None,
) -> str:
    """Convert OpenAI-style messages array to DeepSeek prompt format.

    Args:
        messages: List of OpenAI-style messages with role and content
        tools: Optional OpenAI tools specification
        tool_choice: Controls which tools the model may call (default "auto")
        parallel_tool_calls: Whether to allow parallel tool calls (default True)

    Returns:
        Formatted prompt string for DeepSeek API
    """
    prompt_parts = []
    system_parts = []

    # Preprocess tools based on tool_choice
    effective_tools, tool_choice_info = _preprocess_tools(tools, tool_choice)

    # Log degradation if any
    if tool_choice_info["degraded"]:
        logger.warning(
            f"[messages] tool_choice degraded: reason={tool_choice_info['reason']}, "
            f"missing_name={tool_choice_info.get('missing_name')}"
        )

    for msg in messages:
        role = msg.get("role", "")
        content = msg.get("content")
        text = extract_text_content(content)

        if role == "system":
            system_parts.append(text)
        elif role == "user":
            prompt_parts.append(f"User: {text}")
        elif role == "assistant":
            tool_calls = msg.get("tool_calls")
            if tool_calls:
                # Assistant called tools - use unified [TOOL🛠️] format
                tool_calls_json = []
                for tc in tool_calls:
                    func = tc.get("function", {})
                    name = func.get("name", "")
                    args = func.get("arguments", "")
                    # args may be a JSON string or dict
                    if isinstance(args, str):
                        try:
                            args = json.loads(args)
                        except Exception:
                            pass
                    tool_calls_json.append({"name": name, "arguments": args})
                tool_calls_str = json.dumps(tool_calls_json, ensure_ascii=False)
                prompt_parts.append(f"Assistant: {TOOL_START_MARKER}{tool_calls_str}{TOOL_END_MARKER}")
            else:
                prompt_parts.append(f"Assistant: {text}")
        elif role == "tool":
            # Tool result
            tool_id = msg.get("tool_call_id", "")
            prompt_parts.append(f"\nTool: id={tool_id}\n```\n{text}\n```")

    # Inject tools into system instruction
    if effective_tools:
        tools_lines = []
        for t in effective_tools:
            func = t.get('function', {})
            name = func.get('name')
            desc = func.get('description') or ''
            params = func.get('parameters', {})
            props = params.get('properties', {})

            param_desc = ""
            if props:
                param_lines = []
                for pname, pbody in props.items():
                    ptype = pbody.get('type', 'any')
                    pdesc = pbody.get('description', '')
                    required = pname in params.get('required', [])
                    req_mark = "*" if required else ""

                    # Collect extra fields from property (excluding type, description)
                    extra = {k: v for k, v in pbody.items() if k not in ('type', 'description')}
                    extra_str = f" [{', '.join(f'{k}={v}' for k, v in extra.items())}]" if extra else ""

                    param_lines.append(f"  - {pname}{req_mark} ({ptype}): {pdesc}{extra_str}")
                param_desc = "\n  Parameters:\n" + "\n".join(param_lines)

                if not params.get('additionalProperties', True):
                    param_desc += "\n  Note: Additional parameters are not allowed."

            schema_json = ""
            if params:
                schema_json = "\n```json\n" + json.dumps(params, ensure_ascii=False) + "\n```\n"

            strict_notice = ""
            if func.get('strict', False):
                strict_notice = "\n**Strict Mode**: Function calls MUST exactly match the specified schema. Do NOT add fields not defined, do not omit required fields, do not use values outside enum list."

            tools_lines.append(f"- {name}: {desc}{schema_json}{param_desc}{strict_notice}")

        tools_prompt = "## Available Tools\n" + "\n".join(tools_lines)
        tools_prompt += """

## Response Format
- **User**: human input (you receive this)
- **Assistant**: YOUR response (you output this)
- **Tool**: tool execution result (you receive this after calling tools)

## Tool Usage
You can explain your reasoning before using tools. When you need to call tools, respond with:
[TOOL🛠️][{"name": "function_name", "arguments": {"param": "value"}}, {"name": "another_function", "arguments": {"param": "value"}}][/TOOL🛠️]

**IMPORTANT**:
1. Only use [TOOL🛠️]...[/TOOL🛠️] tags for tool calls.
2. If you need to call multiple tools, put them all in a single [TOOL🛠️]...[/TOOL🛠️] array.
"""
        system_parts.append(tools_prompt)

    # Inject response_format constraint into system prompt
    if response_format:
        fmt_type = response_format.get("type")
        if fmt_type == "json_object":
            system_parts.append(
                "## Response Format\n"
                "You MUST respond ONLY with a valid JSON object. "
                "Do not include any text before or after the JSON."
            )
        elif fmt_type == "json_schema":
            schema = response_format.get("json_schema", {})
            schema_name = schema.get("name", "unknown")
            schema_schema = schema.get("schema", {})
            schema_json = json.dumps(schema_schema, ensure_ascii=False)
            system_parts.append(
                f"## Response Format\n"
                f"You MUST respond ONLY with a valid JSON object conforming to the schema below.\n"
                f"Schema name: {schema_name}\n"
                f"```json\n{schema_json}\n```"
            )

    # Build system instruction block
    if system_parts:
        prompt_parts.insert(0, "[System Instruction]\n" + "\n---\n".join(system_parts) + "\n---")

    # Add separator and REMINDER before Assistant output
    if effective_tools is not None:
        reminder_parts = ["When you need to call tools, you MUST use the [TOOL🛠️]...[/TOOL🛠️] tags."]

        if tool_choice == "required":
            reminder_parts.append("**You MUST call at least one tool before responding.**")

        if not parallel_tool_calls and not (tool_choice_info["degraded"] and tool_choice_info["reason"] == "tool_not_found"):
            reminder_parts.append("**Call only ONE tool at a time.**")

        reminder_text = " ".join(reminder_parts)
        prompt_parts.append(f"\n---\nAbove is our conversation history.\n\n[REMINDER] {reminder_text}")

    elif effective_tools is None and tool_choice_info["degraded"]:
        reason = tool_choice_info["reason"]
        if reason == "no_tools_available":
            prompt_parts.append("\n---\nAbove is our conversation history.\n\n[REMINDER] The requested tool operation requires available tools, but none were provided. Inform the user that no tools are available.")
        elif reason == "tool_not_found":
            missing = tool_choice_info.get("missing_name", "the requested tool")
            prompt_parts.append(f"\n---\nAbove is our conversation history.\n\n[REMINDER] The requested tool \"{missing}\" is not available. Inform the user that this tool is not available.")

    return "\n\n".join(prompt_parts)

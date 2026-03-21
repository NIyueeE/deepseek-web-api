"""OpenAI /v1/chat/completions endpoint."""

import json
import re
import time
import uuid
from typing import List, Optional, Union

import httpx
from fastapi import APIRouter, Request, HTTPException
from ...core.session_store import SessionStore
from fastapi.responses import StreamingResponse, JSONResponse
from pydantic import BaseModel

from ...core.logger import logger

_PROXY_BASE_URL = "http://localhost:5001"
TOOL_START_MARKER = "[TOOL🛠️]"
TOOL_END_MARKER = "[/TOOL🛠️]"
TOOL_JSON_PATTERN = re.compile(r'\[TOOL🛠️\](.*?)\[/TOOL🛠️\]', re.DOTALL)
# Sliding window for tool buffer: end marker length + 3 chars lookahead
TOOL_BUFFER_WINDOW = len(TOOL_END_MARKER) + 3

router = APIRouter()


class ChatCompletionRequest(BaseModel):
    model: str = "deepseek-web-chat"
    messages: List[dict]
    stream: bool = False
    temperature: Optional[float] = None
    search_enabled: bool = False
    thinking_enabled: bool = True
    tools: Optional[List[dict]] = None


def extract_text_content(content: Union[str, List, None]) -> str:
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


def convert_messages_to_prompt(messages: List[dict], tools: Optional[List[dict]] = None) -> str:
    prompt_parts = []
    system_parts = []

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
                # Assistant called tools
                tool_calls_text = []
                for tc in tool_calls:
                    func = tc.get("function", {})
                    name = func.get("name", "")
                    args = func.get("arguments", "")
                    tool_calls_text.append(f"{name}: {args}")
                prompt_parts.append(f"Assistant: [TOOL_CALLS] {', '.join(tool_calls_text)}")
            else:
                prompt_parts.append(f"Assistant: {text}")
        elif role == "tool":
            # Tool result
            tool_id = msg.get("tool_call_id", "")
            prompt_parts.append(f"[TOOL_RESULT id={tool_id}] {text}")

    # Inject tools into system instruction
    if tools:
        tools_lines = []
        for t in tools:
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

            tools_lines.append(f"- {name}: {desc}{param_desc}")

        tools_prompt = "## Available Tools\n" + "\n".join(tools_lines)
        tools_prompt += """

## Tool Usage
If you need to use tools, respond with:
[TOOL🛠️][{"name": "function_name", "arguments": {"param": "value"}}, {"name": "another_function", "arguments": {"param": "value"}}][/TOOL🛠️]

You can put multiple tool calls in a single [TOOL🛠️] tag as a JSON array. Otherwise answer directly.
"""
        system_parts.append(tools_prompt)

    # Build system instruction block
    if system_parts:
        prompt_parts.insert(0, "[System Instruction]\n" + "\n---\n".join(system_parts) + "\n---")

    prompt_parts.append("Assistant: ")
    return "\n\n".join(prompt_parts)


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


def _convert_items_to_tool_calls(items: list, available_tools: list) -> list:
    """Convert parsed JSON items to OpenAI tool_calls format. Returns empty list if no valid calls."""
    tool_calls = []
    for item in items:
        tool_name = item.get("name")
        arguments = item.get("arguments", {})
        if not tool_name:
            continue
        # Validate tool exists
        if not any(t.get("function", {}).get("name") == tool_name for t in available_tools):
            continue
        tool_calls.append(_build_tool_call(tool_name, arguments))
    return tool_calls


def extract_json_tool_calls(text: str, available_tools: List[dict]):
    """Extract and validate JSON tool calls from response text.

    Model returns: [{"name": "func_name", "arguments": {...}}, ...] or {"name": "func_name", "arguments": {...}}
    Service adds: index, id, type
    """
    tool_calls = []

    for match in TOOL_JSON_PATTERN.finditer(text):
        try:
            obj = json.loads(match.group(1))
            items = obj if isinstance(obj, list) else [obj]
            for item in items:
                tool_name = item.get("name")
                arguments = item.get("arguments", {})
                if not tool_name:
                    continue
                if not any(t.get("function", {}).get("name") == tool_name for t in available_tools):
                    logger.warning(f"Unknown tool: {tool_name}")
                    continue
                tc = _build_tool_call(tool_name, arguments)
                tc["index"] = len(tool_calls)
                tool_calls.append(tc)
        except json.JSONDecodeError:
            continue

    cleaned_text = TOOL_JSON_PATTERN.sub('', text)
    return cleaned_text.strip(), tool_calls


def convert_tool_json_to_openai(json_str: str, available_tools: List[dict]):
    """Convert tool JSON from model format to OpenAI tool_calls format.

    Handles both single object: {"name": "func", "arguments": {...}}
    and array: [{"name": "func1", "arguments": {...}}, {"name": "func2", "arguments": {...}}]
    """
    try:
        obj = json.loads(json_str)
        items = obj if isinstance(obj, list) else [obj]
        tool_calls = []
        for item in items:
            tool_name = item.get("name")
            arguments = item.get("arguments", {})
            if not tool_name:
                continue
            if not any(t.get("function", {}).get("name") == tool_name for t in available_tools):
                continue
            tc = _build_tool_call(tool_name, arguments)
            tc["index"] = len(tool_calls)
            tool_calls.append(tc)
        return tool_calls if tool_calls else None
    except json.JSONDecodeError:
        return None


async def delete_session(session_id: str):
    if not session_id:
        return
    SessionStore.get_instance().delete_session(session_id)
    async with httpx.AsyncClient() as client:
        await client.post(f"{_PROXY_BASE_URL}/v0/chat/delete", json={"chat_session_id": session_id})


async def stream_generator(prompt: str, model_name: str, search_enabled: bool, thinking_enabled: bool, tools: Optional[List[dict]] = None):
    """Stream DeepSeek SSE and convert to OpenAI SSE format."""
    req_id = f"chatcmpl-{uuid.uuid4().hex}"
    created_time = int(time.time())
    session_id_to_delete = None

    def make_chunk(content=None, reasoning=None, finish_reason=None, tool_calls=None):
        delta = {"content": content, "reasoning_content": reasoning}
        if tool_calls:
            delta["tool_calls"] = tool_calls
        choice = {"index": 0, "delta": delta}
        if finish_reason:
            choice["finish_reason"] = finish_reason

        chunk_data = {
            "id": req_id,
            "object": "chat.completion.chunk",
            "created": created_time,
            "model": model_name,
            "choices": [choice],
        }
        chunk_str = f"data: {json.dumps(chunk_data, ensure_ascii=False)}\n\n"
        logger.debug(f"Yielding chunk: {chunk_str}")
        return chunk_str

    payload = {
        "prompt": prompt,
        "search_enabled": search_enabled,
        "thinking_enabled": thinking_enabled,
        "ref_file_ids": [],
    }

    append_count = [0]
    in_output = [False]
    tool_tail = ""
    in_tool_buffer = False
    had_tool_call = False

    # If thinking is disabled, all content goes directly to output (no reasoning)
    if not thinking_enabled:
        in_output = [True]

    async with httpx.AsyncClient(timeout=300.0) as client:
        try:
            async with client.stream(
                "POST", f"{_PROXY_BASE_URL}/v0/chat/completion", json=payload
            ) as response:
                session_id_to_delete = response.headers.get("x-chat-session-id")
                logger.debug(f" Got session_id from header: {session_id_to_delete}")

                async for line in response.aiter_lines():
                    if not line.startswith("data: "):
                        continue

                    data_str = line[6:].strip()
                    if not data_str or data_str == "{}":
                        continue

                    try:
                        data = json.loads(data_str)
                    except json.JSONDecodeError:
                        continue

                    v = data.get("v")
                    o = data.get("o")
                    p = data.get("p")
                    logger.debug(f"RAW data_str: {repr(data_str[:200])}, v: {repr(str(v)[:100])}, o: {repr(o)}, p: {repr(p)}")

                    # Handle v as object with response.fragments (initial thinking content)
                    if isinstance(v, dict):
                        response_obj = v.get("response", {})
                        fragments = response_obj.get("fragments", [])
                        logger.debug(f" dict v, fragments count: {len(fragments)}")
                        for fragment in fragments:
                            frag_type = fragment.get("type")
                            content = fragment.get("content", "")
                            logger.debug(f" fragment type={frag_type}, content={repr(content)[:100]}")
                            if frag_type == "THINK" and content:
                                chunk = make_chunk(reasoning=content)
                                logger.debug(f" YIELD REASONING: {repr(chunk)[:100]}")
                                yield chunk
                        continue

                    if not isinstance(v, str) or v in ("FINISHED", "SEARCHING"):
                        continue

                    # APPEND event: count it, switch to output after 2nd
                    if o == "APPEND":
                        append_count[0] += 1
                        if append_count[0] >= 2:
                            in_output[0] = True

                    # Handle tool markers in streaming output
                    if in_output[0]:
                        if tools:
                            tool_tail += str(v)

                            if not in_tool_buffer:
                                # Check for start marker
                                start_idx = tool_tail.find(TOOL_START_MARKER)
                                if start_idx != -1:
                                    # Yield content before start marker
                                    before_start = tool_tail[:start_idx]
                                    for char in before_start:
                                        yield make_chunk(content=char)
                                    # Keep only from start marker onwards in buffer
                                    tool_tail = tool_tail[start_idx:]
                                    in_tool_buffer = True
                                    logger.debug(f"Entering tool buffer mode, tool_tail={repr(tool_tail)}")
                                else:
                                    # No start marker yet, yield fallen chars
                                    if len(tool_tail) > TOOL_BUFFER_WINDOW:
                                        fallen = tool_tail[:-TOOL_BUFFER_WINDOW]
                                        for char in fallen:
                                            yield make_chunk(content=char)
                                    tool_tail = tool_tail[-TOOL_BUFFER_WINDOW:]
                            else:
                                # In buffer mode, keep all content until end marker found
                                end_idx = tool_tail.find(TOOL_END_MARKER)
                                if end_idx != -1:
                                    # Extract JSON
                                    json_start_idx = tool_tail.find(TOOL_START_MARKER)
                                    if json_start_idx != -1 and end_idx > json_start_idx:
                                        json_str = tool_tail[json_start_idx + len(TOOL_START_MARKER):end_idx]
                                        tool_calls_result = convert_tool_json_to_openai(json_str, tools)
                                        if tool_calls_result:
                                            for tc in tool_calls_result:
                                                yield make_chunk(tool_calls=[tc])
                                                had_tool_call = True
                                    # Yield content after end marker
                                    after_end = tool_tail[end_idx + len(TOOL_END_MARKER):]
                                    for char in after_end:
                                        yield make_chunk(content=char)
                                    in_tool_buffer = False
                                    tool_tail = ""
                                else:
                                    # Keep buffering (no trim in buffer mode to preserve start marker)
                                    pass
                        else:
                            yield make_chunk(content=v)
                    else:
                        yield make_chunk(reasoning=v)
                    continue
        finally:
            # Cleanup after stream ends and [DONE] is consumed
            logger.debug(f" Cleanup: session_id_to_delete={session_id_to_delete}")
            if session_id_to_delete:
                await delete_session(session_id_to_delete)

    # Send finish reason
    finish_reason = "tool_calls" if had_tool_call else "stop"
    chunk = make_chunk(finish_reason=finish_reason)
    yield chunk
    logger.debug("Yielding [DONE]")
    # Stream ended
    yield "data: [DONE]\n\n"


@router.post("/v1/chat/completions")
async def chat_completions(request: Request):
    body = await request.body()

    try:
        data = json.loads(body)
    except json.JSONDecodeError:
        raise HTTPException(status_code=400, detail="Invalid JSON")

    validated = ChatCompletionRequest(**data)
    logger.debug(f"Request payload: {json.dumps(data, ensure_ascii=False, indent=2)}")
    prompt = convert_messages_to_prompt(validated.messages, validated.tools)
    logger.debug(f"Constructed prompt:\n{prompt}")

    # Model-specific overrides: reasoning model gets thinking, others don't
    if "reasoner" in validated.model:
        search_enabled = False
        thinking_enabled = True
    else:
        search_enabled = False
        thinking_enabled = False

    if validated.stream:
        return StreamingResponse(
            stream_generator(prompt, validated.model, search_enabled, thinking_enabled, validated.tools),
            media_type="text/event-stream",
        )

    # Non-streaming: collect all content
    content_chunks = []
    reasoning_chunks = []
    all_tool_calls = []

    generator = stream_generator(prompt, validated.model, search_enabled, thinking_enabled, validated.tools)
    async for chunk_str in generator:
        if chunk_str == "data: [DONE]\n\n":
            break
        try:
            chunk_json = json.loads(chunk_str[6:])
            delta = chunk_json.get("choices", [{}])[0].get("delta", {})
            if delta.get("content"):
                content_chunks.append(delta["content"])
            if delta.get("reasoning_content"):
                reasoning_chunks.append(delta["reasoning_content"])
            if delta.get("tool_calls"):
                all_tool_calls.extend(delta["tool_calls"])
        except (json.JSONDecodeError, IndexError, KeyError):
            pass

    full_content = "".join(content_chunks)
    full_reasoning = "".join(reasoning_chunks)

    logger.debug(f"Non-streaming collected: content_len={len(full_content)}, reasoning_len={len(full_reasoning)}, tool_calls_count={len(all_tool_calls)}, content_preview={repr(full_content[:200])}")

    # Extract tool calls from content if none found
    if not all_tool_calls and validated.tools:
        logger.debug(f"No tool_calls in chunks, trying extract_json_tool_calls on: {repr(full_content[:500])}")
        full_content, all_tool_calls = extract_json_tool_calls(full_content, validated.tools)
        logger.debug(f"After extraction: content_len={len(full_content)}, tool_calls_count={len(all_tool_calls)}")
    finish_reason = "tool_calls" if all_tool_calls else "stop"

    message = {
        "role": "assistant",
        "content": full_content,
        "reasoning_content": full_reasoning if full_reasoning else None,
    }
    if all_tool_calls:
        message["tool_calls"] = all_tool_calls

    logger.debug(f"Non-streaming final message: {json.dumps(message, ensure_ascii=False)[:500]}")
    return JSONResponse(
        content={
            "id": f"chatcmpl-{uuid.uuid4().hex}",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": validated.model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        }
    )

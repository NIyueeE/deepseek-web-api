"""Stream generation service for chat completions.

Converts DeepSeek SSE responses to OpenAI SSE format.
"""

import asyncio
import json
import time
import uuid
from typing import Optional, List, TYPE_CHECKING

if TYPE_CHECKING:
    from .session_pool import StatelessSession

from ....core.logger import logger
from ...v0_service import stream_chat_completion, stream_edit_message
from .tools import (
    TOOL_START_MARKER,
    TOOL_END_MARKER,
    get_stream_window,
    convert_tool_json_to_openai,
)

_UNSET_USAGE = object()

TOOL_BUFFER_TIMEOUT = 3.0  # seconds of silence before forcing end in tool buffer mode


def _flush_with_stop(buffer: str, stop_sequences: list[str]) -> tuple[str, bool]:
    """Check buffer for stop sequences. Truncate at first occurrence.

    Returns: (content_to_yield, stop_triggered)
    - content_to_yield: text BEFORE the stop sequence (empty if stop triggered before any text)
    - stop_triggered: True if a stop sequence was found and truncated
    """
    if not stop_sequences:
        return buffer, False

    earliest_idx = len(buffer)
    for seq in stop_sequences:
        idx = buffer.find(seq)
        if idx != -1 and idx < earliest_idx:
            earliest_idx = idx

    if earliest_idx < len(buffer):
        return buffer[:earliest_idx], True
    return buffer, False


def _extract_complete_sse_events(buffer: str) -> tuple[list[str], str]:
    """Extract complete SSE events from a cumulative buffer."""
    normalized = buffer.replace("\r\n", "\n")
    events = []
    while "\n\n" in normalized:
        event, normalized = normalized.split("\n\n", 1)
        if event.strip():
            events.append(event)
    return events, normalized


async def stream_generator(
    prompt: str,
    model_name: str,
    search_enabled: bool,
    thinking_enabled: bool,
    tools: Optional[List[dict]] = None,
    session: "StatelessSession" = None,
    stop_sequences: Optional[List[str]] = None,
    include_usage: bool = False,
):
    """Stream DeepSeek SSE and convert to OpenAI SSE format.

    Args:
        prompt: The prompt to send
        model_name: Model name for response
        search_enabled: Enable web search
        thinking_enabled: Enable thinking/reasoning
        tools: Available tools (OpenAI format)
        session: StatelessSession from the pool. If session.is_initialized is False,
                 uses completion to initialize message_id=1 first.
    """
    # Choose the appropriate stream function based on session state
    if session and not session.is_initialized:
        # First request to this session - use completion to create message_id=1
        stream_func = stream_chat_completion
        logger.info(f"[stream_generator] session {session.chat_session_id[:8]}... initializing with completion")
    else:
        # Session already initialized - use edit_message with message_id=1
        stream_func = stream_edit_message
        logger.debug(f"[stream_generator] session {session.chat_session_id[:8]}... using edit_message")

    req_id = f"chatcmpl-{uuid.uuid4().hex}"
    created_time = int(time.time())

    def make_chunk(content=None, reasoning=None, finish_reason=None, tool_calls=None, role=None, usage=_UNSET_USAGE, final=False):
        delta = {"content": content, "reasoning_content": reasoning}
        if role is not None:
            delta["role"] = role
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

        if final:
            # 末尾 usage chunk: choices=[], usage=usage dict
            chunk_data["choices"] = []
            chunk_data["usage"] = usage
        elif usage is not _UNSET_USAGE:
            # include_usage=true 时的普通 chunk: usage=null
            chunk_data["usage"] = None

        chunk_str = f"data: {json.dumps(chunk_data, ensure_ascii=False)}\n\n"
        logger.debug(f"Yielding chunk: {chunk_str}")
        return chunk_str

    # State machine: None -> reasoning/content (set by p field)
    current_mode = None
    stream_buff = ""
    in_stream_buffer = False
    had_tool_call = False
    force_end = False  # Flag to indicate we should stop yielding to client after tool calls
    client_stream_closed = False
    extra_prefix = ""  # Prefetched content from edit_message nested dict format
    sse_buffer = ""
    buffer_window = get_stream_window(stop_sequences or [])

    yield make_chunk(role="assistant", usage=include_usage if include_usage else _UNSET_USAGE)

    # Wrap the async iterator with per-chunk timeout
    iterable = stream_func(
        prompt=prompt,
        chat_session_id=session.chat_session_id if session else None,
        search_enabled=search_enabled,
        thinking_enabled=thinking_enabled,
    )
    ait = iterable.__aiter__()
    while True:
        try:
            line = await asyncio.wait_for(ait.__anext__(), timeout=TOOL_BUFFER_TIMEOUT)
        except asyncio.TimeoutError:
            # 3s elapsed with no chunk — force stream end
            logger.warning("[stream] tool buffer timeout, forcing end")
            if in_stream_buffer and stream_buff:
                end_idx = stream_buff.find(TOOL_END_MARKER)
                json_start_idx = stream_buff.find(TOOL_START_MARKER)
                if end_idx != -1 and json_start_idx != -1 and end_idx > json_start_idx:
                    json_str = stream_buff[json_start_idx + len(TOOL_START_MARKER):end_idx]
                    tool_calls_result = convert_tool_json_to_openai(json_str, tools)
                    if tool_calls_result:
                        for tc in tool_calls_result:
                            yield make_chunk(tool_calls=[tc], usage=include_usage if include_usage else _UNSET_USAGE)
                # Yield remaining content after end marker (if any)
                after_end = stream_buff[end_idx + len(TOOL_END_MARKER):] if end_idx != -1 else stream_buff
                if after_end:
                    to_yield, stopped = _flush_with_stop(after_end, stop_sequences or [])
                    if to_yield:
                        yield make_chunk(content=to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
            yield make_chunk(finish_reason="tool_calls" if had_tool_call else "stop",
                             usage=include_usage if include_usage else _UNSET_USAGE)
            if include_usage:
                yield make_chunk(usage={"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}, final=True)
            yield "data: [DONE]\n\n"
            break
        except StopAsyncIteration:
            break

        if isinstance(line, bytes):
            line = line.decode("utf-8")
        sse_buffer += line
        events, sse_buffer = _extract_complete_sse_events(sse_buffer)
        for raw_event in events:
            raw_event = raw_event.strip()
            if not raw_event:
                continue

            # Each event may contain \n-separated parts like "event: xxx\ndata: {...}"
            for part in raw_event.split("\n"):
                part = part.strip()
                if not part or part == "{}":
                    continue
                if part.startswith("data: "):
                    part = part[6:]
                else:
                    continue  # Skip "event: xxx" lines

                try:
                    data = json.loads(part)
                except json.JSONDecodeError:
                    logger.warning(f"JSON parse failed for part: {repr(part[:200])}")
                    continue

                v = data.get("v")
                p = data.get("p")
                logger.debug(f"RAW event: {repr(part[:100])}, v={repr(str(v)[:50])}, p={repr(p)}")

                # Check for stream end
                if p and "status" in p and v == "FINISHED":
                    # If we already sent [DONE] for tool calls, just drain remaining data
                    if force_end:
                        logger.debug("Draining remaining stream data after tool calls")
                        break
                    # Flush remaining extra_prefix before finishing (unconditional)
                    if extra_prefix:
                        prefix_to_yield, stopped = _flush_with_stop(extra_prefix, stop_sequences or [])
                        if prefix_to_yield:
                            yield make_chunk(content=prefix_to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
                        if stopped:
                            force_end = True
                        extra_prefix = ""
                    # Flush remaining stream_buff before finishing
                    if in_stream_buffer and stream_buff:
                        end_idx = stream_buff.find(TOOL_END_MARKER)
                        if end_idx != -1:
                            json_start_idx = stream_buff.find(TOOL_START_MARKER)
                            if json_start_idx != -1 and end_idx > json_start_idx:
                                json_str = stream_buff[json_start_idx + len(TOOL_START_MARKER):end_idx]
                                tool_calls_result = convert_tool_json_to_openai(json_str, tools)
                                if tool_calls_result:
                                    for tc in tool_calls_result:
                                        yield make_chunk(tool_calls=[tc], usage=include_usage if include_usage else _UNSET_USAGE)
                                        had_tool_call = True
                        after_end = stream_buff[end_idx + len(TOOL_END_MARKER):]
                        to_yield, stopped = _flush_with_stop(after_end, stop_sequences or [])
                        if to_yield:
                            yield make_chunk(content=to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
                        if stopped:
                            force_end = True
                    elif stream_buff:
                        # Not in buffer mode but have remaining content - flush it
                        to_yield, stopped = _flush_with_stop(stream_buff, stop_sequences or [])
                        if to_yield:
                            yield make_chunk(content=to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
                        if stopped:
                            force_end = True
                    break

                # Skip yielding anything to client if we've already force-ended
                if force_end:
                    continue

                # Handle edit_message SSE format where v is a nested dict:
                # {"response":{"fragments":[{"type":"THINK","content":"We"}]}}
                # or {"response":{"content":"actual content"}}
                if isinstance(v, dict):
                    logger.debug(f"[dict v detected] data={repr(data)}, v={repr(v)}")
                    # Still need to set current_mode based on p even for dict v
                    if p and "thinking_content" in p:
                        current_mode = "reasoning"
                    elif p and "content" in p:
                        current_mode = "output"
                    extracted = None
                    if "response" in v:
                        resp = v["response"]
                        if isinstance(resp, dict) and "fragments" in resp:
                            fragments = resp["fragments"]
                            if isinstance(fragments, list) and fragments:
                                first_fragment = fragments[0]
                                if isinstance(first_fragment, dict) and "content" in first_fragment:
                                    extracted = str(first_fragment["content"])
                        elif isinstance(resp, dict) and "content" in resp:
                            extracted = str(resp["content"])
                    if extracted:
                        extra_prefix += extracted
                        logger.debug(f"Prefetched extra_prefix={repr(extra_prefix)}")
                    continue

                if not isinstance(v, str) or v == "SEARCHING":
                    continue

                # Switch mode based on p field (use "in" for flexible matching)
                if p and "thinking_content" in p:
                    current_mode = "reasoning"
                elif p and "content" in p:
                    current_mode = "output"

                # Ignore if no mode set yet (initial state)
                if current_mode is None:
                    logger.debug(f"Ignoring v before mode set: {repr(v)[:50]}")
                    continue

                # Prepend extra_prefix (from nested dict extraction) and clear it
                if extra_prefix:
                    v = extra_prefix + str(v)
                    extra_prefix = ""
                    logger.debug(f"Prepended extra_prefix, v now={repr(v[:50])}")

                # Handle content based on current mode
                if current_mode == "output":
                    if tools:
                        stream_buff += str(v)

                        if not in_stream_buffer:
                            # Check for start marker
                            start_idx = stream_buff.find(TOOL_START_MARKER)
                            if start_idx != -1:
                                # Yield content before start marker (with stop detection)
                                before_start = stream_buff[:start_idx]
                                to_yield, stopped = _flush_with_stop(before_start, stop_sequences or [])
                                if to_yield:
                                    yield make_chunk(content=to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
                                if stopped:
                                    force_end = True
                                # Keep only from start marker onwards in buffer
                                stream_buff = stream_buff[start_idx:]
                                in_stream_buffer = True
                                logger.debug(f"Entering tool buffer mode, stream_buff={repr(stream_buff)}")
                            else:
                                # No start marker yet, yield fallen chars
                                if len(stream_buff) > buffer_window:
                                    fallen = stream_buff[:-buffer_window]
                                    to_yield, stopped = _flush_with_stop(fallen, stop_sequences or [])
                                    if to_yield:
                                        yield make_chunk(content=to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
                                    if stopped:
                                        force_end = True
                                stream_buff = stream_buff[-buffer_window:]
                        else:
                            # In buffer mode, keep all content until end marker found
                            end_idx = stream_buff.find(TOOL_END_MARKER)
                            if end_idx != -1:
                                # Extract JSON
                                json_start_idx = stream_buff.find(TOOL_START_MARKER)
                                if json_start_idx != -1 and end_idx > json_start_idx:
                                    json_str = stream_buff[json_start_idx + len(TOOL_START_MARKER):end_idx]
                                    tool_calls_result = convert_tool_json_to_openai(json_str, tools)
                                    if tool_calls_result:
                                        for tc in tool_calls_result:
                                            yield make_chunk(tool_calls=[tc], usage=include_usage if include_usage else _UNSET_USAGE)
                                            had_tool_call = True
                                        # Successfully parsed tool calls - send finish and [DONE]
                                        yield make_chunk(finish_reason="tool_calls", usage=include_usage if include_usage else _UNSET_USAGE)
                                        yield "data: [DONE]\n\n"
                                        logger.debug("Tool calls parsed, forcing stream end")
                                        force_end = True
                                        client_stream_closed = True
                                # Even if parsing failed, we still force end at [/TOOL🛠️]
                                if force_end:
                                    # Continue consuming remaining data but don't yield to client
                                    continue
                            else:
                                # Keep buffering (no trim in buffer mode to preserve start marker)
                                pass
                    else:
                        to_yield, stopped = _flush_with_stop(v, stop_sequences or [])
                        if to_yield:
                            yield make_chunk(content=to_yield, usage=include_usage if include_usage else _UNSET_USAGE)
                        if stopped:
                            force_end = True
                else:
                    # reasoning mode
                    yield make_chunk(reasoning=v, usage=include_usage if include_usage else _UNSET_USAGE)

    if not client_stream_closed:
        # Send finish reason once unless we already closed the client stream on tool_calls
        yield make_chunk(finish_reason="tool_calls" if had_tool_call else "stop", usage=include_usage if include_usage else _UNSET_USAGE)
        logger.debug("Yielding [DONE]")
        if include_usage:
            yield make_chunk(usage={"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}, final=True)
        yield "data: [DONE]\n\n"

    # Mark session as initialized after successful completion (message_id=1 now exists)
    if session and not session.is_initialized:
        session.is_initialized = True
        logger.info(f"[stream_generator] session {session.chat_session_id[:8]}... marked as initialized")

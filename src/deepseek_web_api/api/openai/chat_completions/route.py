"""OpenAI /v1/chat/completions endpoint - thin router."""

import asyncio
import json
import time
import uuid
from typing import List, Optional, Union

from fastapi import APIRouter, Request, HTTPException
from fastapi.responses import StreamingResponse, JSONResponse
from pydantic import BaseModel

from ....core.logger import logger
from .messages import convert_messages_to_prompt
from .service import stream_generator
from .session_pool import get_pool, SessionPoolFullError
from ....api.v0_service import RateLimitError


router = APIRouter()


class ChatCompletionRequest(BaseModel):
    """OpenAI-compatible chat completion request.

    DeepSeek-specific parameters (search_enabled, thinking_enabled) can be
    passed via the OpenAI SDK's extra_body mechanism:

        client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[...],
            extra_body={
                "search_enabled": True,
                "thinking_enabled": False,
            }
        )

    Proxy-layer parameters (not forwarded to DeepSeek):
        - tool_choice: Controls which tools the model may call (default "auto")
        - parallel_tool_calls: Whether to allow parallel tool calls (default True)
    """
    model: str = "deepseek-web-chat"
    messages: List[dict]
    stream: bool = False
    tools: Optional[List[dict]] = None
    tool_choice: Optional[Union[str, dict]] = "auto"
    parallel_tool_calls: Optional[bool] = True
    extra_body: Optional[dict] = None


def _stream_error_chunks(message: str):
    error_payload = {
        "error": {
            "message": message,
            "type": "server_error",
        }
    }
    yield f"data: {json.dumps(error_payload, ensure_ascii=False)}\n\n"
    yield "data: [DONE]\n\n"


@router.post("/v1/chat/completions")
async def chat_completions(request: Request):
    """OpenAI-compatible /v1/chat/completions endpoint.

    Uses stateless session pool internally:
    - Each session tracks if message_id=1 is initialized
    - First request to a session uses completion, subsequent use edit_message
    - Client doesn't need to track session_id - we handle it internally
    """
    body = await request.body()

    try:
        data = json.loads(body)
    except json.JSONDecodeError:
        raise HTTPException(status_code=400, detail="Invalid JSON")

    validated = ChatCompletionRequest(**data)
    logger.debug(f"Request payload: {json.dumps(data, ensure_ascii=False, indent=2)}")
    prompt = convert_messages_to_prompt(
        validated.messages,
        validated.tools,
        validated.tool_choice,
        validated.parallel_tool_calls,
    )
    logger.debug(f"Constructed prompt:\n{prompt}")

    # Extract DeepSeek-specific parameters from extra_body
    extra = validated.extra_body or {}
    search_enabled = extra.get("search_enabled", False)
    # Model-specific override: reasoning model enables thinking by default
    thinking_enabled = extra.get("thinking_enabled", "reasoner" in validated.model)

    pool = await get_pool()

    _MAX_SESSION_RETRIES = 3
    _RETRY_DELAY = 1.0
    STREAM_BUFFER_THRESHOLD = 100

    if validated.stream:
        async def stream_with_pool():
            """Generator that acquires session from pool, streams, and releases."""
            for session_retry in range(_MAX_SESSION_RETRIES):
                try:
                    session = await pool.acquire()
                except SessionPoolFullError as e:
                    logger.warning(f"[stream] pool exhausted: {e}")
                    async for chunk in _emit_stream_error(
                        "Service temporarily unavailable: all DeepSeek sessions are busy, please retry later"
                    ):
                        yield chunk
                    return
                except Exception as e:
                    logger.error(f"[stream] failed to acquire session: {e}")
                    async for chunk in _emit_stream_error(
                        f"Failed to create DeepSeek session: {type(e).__name__}"
                    ):
                        yield chunk
                    return
                logger.info(f"[stream] acquired session {session.chat_session_id[:8]}..., is_initialized={session.is_initialized}")

                buffered = []
                started_yielding = False
                success = False
                error_session = False  # If True, session will be marked for re-init

                try:
                    async for chunk in stream_generator(
                        prompt, validated.model, search_enabled, thinking_enabled,
                        validated.tools, session
                    ):
                        if not started_yielding:
                            buffered.append(chunk)
                            # Buffer chunks until threshold reached.
                            # This avoids sending partial SSL/writes - if an error occurs
                            # before reaching the threshold, buffered content is discarded
                            # (it may be incomplete/corrupted). Once threshold is reached,
                            # we assume the connection is stable and yield chunks directly.
                            if len(buffered) >= STREAM_BUFFER_THRESHOLD:
                                for b in buffered:
                                    yield b
                                buffered.clear()
                                started_yielding = True
                        else:
                            yield chunk
                    success = True
                    logger.info(f"[stream] session {session.chat_session_id[:8]}... completed successfully")
                    # Stream completed successfully - flush any remaining buffered chunks
                    if buffered:
                        for b in buffered:
                            yield b
                        buffered.clear()
                except RateLimitError as e:
                    # Rate limit is account/IP-wide — retrying with a different session won't help.
                    # The inner retry loop (in stream_chat_completion/stream_edit_message) already
                    # exhausted its attempts; surface the error without touching session state.
                    logger.warning(f"[stream] DeepSeek rate limit exhausted all retries: {e}")
                    if not started_yielding:
                        async for chunk in _emit_stream_error(
                            "DeepSeek rate limit exceeded, please retry later"
                        ):
                            yield chunk
                    return  # No session retry — a different session won't help
                except Exception as e:
                    error_session = True
                    logger.warning(f"[stream] session {session.chat_session_id[:8]}... error: {e}")
                    if session_retry < _MAX_SESSION_RETRIES - 1:
                        logger.warning(f"[stream] retrying with different session ({session_retry + 1}/{_MAX_SESSION_RETRIES})")
                        await asyncio.sleep(_RETRY_DELAY * (2 ** session_retry))
                    else:
                        logger.error(f"[stream] all session retries exhausted: {e}")
                        raise
                finally:
                    # Release session back to pool (mark as needing re-init if error)
                    if session:
                        await pool.release(session, error=error_session)

                # On failure: only flush if we never started yielding.
                # If we were already streaming, buffered content may be incomplete
                # and should not be sent to client.
                if not success and buffered and not started_yielding:
                    for b in buffered:
                        yield b

                if success:
                    break

            # Trigger cleanup of idle sessions
            asyncio.create_task(pool.cleanup_idle())

        return StreamingResponse(
            stream_with_pool(),
            media_type="text/event-stream",
        )

    # Non-streaming: buffer all chunks first, then return
    last_exc = None
    chunks = []

    for session_retry in range(_MAX_SESSION_RETRIES):
        try:
            session = await pool.acquire()
        except SessionPoolFullError as e:
            logger.warning(f"[non-stream] pool exhausted: {e}")
            raise HTTPException(
                status_code=503,
                detail="Service temporarily unavailable: all DeepSeek sessions are busy, please retry later",
            )
        except Exception as e:
            last_exc = e
            logger.error(f"[non-stream] failed to acquire session: {e}")
            break
        logger.info(f"[non-stream] acquired session {session.chat_session_id[:8]}..., is_initialized={session.is_initialized}")

        chunks = []
        error_session = False

        try:
            async for chunk in stream_generator(
                prompt, validated.model, search_enabled, thinking_enabled,
                validated.tools, session
            ):
                chunks.append(chunk)
            break  # Success
        except RateLimitError as e:
            last_exc = e
            logger.warning(f"[non-stream] DeepSeek rate limit exhausted all retries: {e}")
            # Don't retry with a different session — rate limit is account/IP-wide
        except Exception as e:
            error_session = True
            last_exc = e
            logger.warning(f"[non-stream] session {session.chat_session_id[:8]}... error: {e}")
            if session_retry < _MAX_SESSION_RETRIES - 1:
                logger.warning(f"[non-stream] retrying with different session ({session_retry + 1}/{_MAX_SESSION_RETRIES})")
                await asyncio.sleep(_RETRY_DELAY * (2 ** session_retry))
            else:
                logger.error(f"[non-stream] all session retries exhausted: {e}")
        finally:
            if session:
                await pool.release(session, error=error_session)

    # If all retries failed, raise the last exception
    if not chunks:
        if isinstance(last_exc, (RateLimitError, SessionPoolFullError)):
            raise HTTPException(
                status_code=503,
                detail="Service temporarily unavailable: DeepSeek rate limit exceeded, please retry later",
            )
        if last_exc is not None:
            raise last_exc
        raise HTTPException(status_code=502, detail="DeepSeek returned no completion chunks")

    # Parse buffered chunks
    content_chunks = []
    reasoning_chunks = []
    all_tool_calls = []
    finish_reason = "stop"

    for chunk_str in chunks:
        if chunk_str == "data: [DONE]\n\n":
            continue
        try:
            chunk_json = json.loads(chunk_str[6:])
            choice = chunk_json.get("choices", [{}])[0]
            delta = choice.get("delta", {})
            if delta.get("content"):
                content_chunks.append(delta["content"])
            if delta.get("reasoning_content"):
                reasoning_chunks.append(delta["reasoning_content"])
            if delta.get("tool_calls"):
                all_tool_calls.extend(delta["tool_calls"])
                finish_reason = "tool_calls"
            if choice.get("finish_reason"):
                finish_reason = choice["finish_reason"]
        except (json.JSONDecodeError, IndexError, KeyError):
            pass

    full_content = "".join(content_chunks)
    full_reasoning = "".join(reasoning_chunks)

    message = {
        "role": "assistant",
        "content": full_content,
        "reasoning_content": full_reasoning if full_reasoning else None,
    }
    if all_tool_calls:
        message["tool_calls"] = all_tool_calls

    # Trigger cleanup of idle sessions
    asyncio.create_task(pool.cleanup_idle())

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


async def _emit_stream_error(message: str):
    for chunk in _stream_error_chunks(message):
        yield chunk

"""Unit tests for service.py - stream generation service."""

import json

import pytest


from deepseek_web_api.api.openai.chat_completions.service import (
    _extract_complete_sse_events,
    stream_generator,
)
from deepseek_web_api.api.openai.chat_completions.session_pool import StatelessSession


def _parse_sse_chunk(chunk: str) -> dict | None:
    if chunk == "data: [DONE]\n\n":
        return None
    return json.loads(chunk[6:])


class TestSseHelpers:
    """Tests for SSE buffering helpers."""

    def test_extract_complete_sse_events_preserves_partial_chunks(self):
        events, rest = _extract_complete_sse_events('data: {"p":"response/content","v":"Hel')
        assert events == []
        assert rest == 'data: {"p":"response/content","v":"Hel'

        events, rest = _extract_complete_sse_events(rest + 'lo"}\n\ndata: {"p":"response/status","v":"FINISHED"}\n\n')
        assert events == [
            'data: {"p":"response/content","v":"Hello"}',
            'data: {"p":"response/status","v":"FINISHED"}',
        ]
        assert rest == ""


class TestStreamGenerator:
    """Tests for stream_generator behavior."""

    @pytest.mark.asyncio
    async def test_stream_generator_handles_split_sse_events(self, monkeypatch):
        async def fake_stream_chat_completion(**kwargs):
            yield b'data: {"p":"response/content","v":"Hel'
            yield b'lo"}\n\ndata: {"p":"response/status","v":"FIN'
            yield b'ISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-1", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "hello",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=None,
                session=session,
            )
        ]

        payloads = [_parse_sse_chunk(chunk) for chunk in chunks if chunk != "data: [DONE]\n\n"]
        content = "".join(
            payload["choices"][0]["delta"].get("content") or ""
            for payload in payloads
        )
        finish_reasons = [
            payload["choices"][0].get("finish_reason")
            for payload in payloads
            if payload["choices"][0].get("finish_reason")
        ]

        assert content == "Hello"
        assert finish_reasons == ["stop"]
        assert chunks.count("data: [DONE]\n\n") == 1
        assert session.is_initialized is True

    @pytest.mark.asyncio
    async def test_stream_generator_sends_single_done_for_tool_calls(self, monkeypatch):
        tool_payload = (
            '[TOOL🛠️]{"name":"get_weather","arguments":{"city":"Beijing"}}[/TOOL🛠️]'
        )

        async def fake_stream_chat_completion(**kwargs):
            yield f'data: {json.dumps({"p": "response/content", "v": tool_payload}, ensure_ascii=False)}\n\n'.encode()
            yield b'data: {"p":"response/status","v":"FINISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-2", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "weather",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=[
                    {
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "description": "Get weather",
                        },
                    }
                ],
                session=session,
            )
        ]

        payloads = [_parse_sse_chunk(chunk) for chunk in chunks if chunk != "data: [DONE]\n\n"]
        finish_reasons = [
            payload["choices"][0].get("finish_reason")
            for payload in payloads
            if payload["choices"][0].get("finish_reason")
        ]
        tool_calls = [
            tool_call
            for payload in payloads
            for tool_call in payload["choices"][0]["delta"].get("tool_calls", [])
        ]

        assert chunks.count("data: [DONE]\n\n") == 1
        assert finish_reasons == ["tool_calls"]
        assert len(tool_calls) == 1
        assert tool_calls[0]["function"]["name"] == "get_weather"

    @pytest.mark.asyncio
    async def test_first_chunk_has_role_assistant(self, monkeypatch):
        async def fake_stream_chat_completion(**kwargs):
            yield b'data: {"p":"response/content","v":"Hello"}\n\n'
            yield b'data: {"p":"response/status","v":"FINISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-role", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "hello",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=None,
                session=session,
            )
        ]

        first_chunk = _parse_sse_chunk(chunks[0])
        assert first_chunk["choices"][0]["delta"]["role"] == "assistant"

    @pytest.mark.asyncio
    async def test_first_chunk_has_usage_null_when_include_usage_true(self, monkeypatch):
        async def fake_stream_chat_completion(**kwargs):
            yield b'data: {"p":"response/content","v":"Hi"}\n\n'
            yield b'data: {"p":"response/status","v":"FINISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-usage", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "hi",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=None,
                session=session,
                include_usage=True,
            )
        ]

        first_chunk = _parse_sse_chunk(chunks[0])
        assert first_chunk.get("usage") is None
        assert first_chunk["choices"][0]["delta"]["role"] == "assistant"

    @pytest.mark.asyncio
    async def test_include_usage_true_appends_null_usage_each_chunk(self, monkeypatch):
        async def fake_stream_chat_completion(**kwargs):
            yield b'data: {"p":"response/content","v":"Hi"}\n\n'
            yield b'data: {"p":"response/status","v":"FINISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-usage2", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "hi",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=None,
                session=session,
                include_usage=True,
            )
        ]

        content_chunks = []
        for chunk in chunks:
            if chunk == "data: [DONE]\n\n":
                continue
            parsed = _parse_sse_chunk(chunk)
            if parsed is None or parsed.get("choices") != []:
                content_chunks.append(chunk)

        for chunk in content_chunks:
            parsed = _parse_sse_chunk(chunk)
            assert parsed.get("usage") is None, f"Expected usage=null but got {parsed.get('usage')}"

    @pytest.mark.asyncio
    async def test_include_usage_true_appends_final_usage_chunk(self, monkeypatch):
        async def fake_stream_chat_completion(**kwargs):
            yield b'data: {"p":"response/content","v":"Hi"}\n\n'
            yield b'data: {"p":"response/status","v":"FINISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-final", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "hi",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=None,
                session=session,
                include_usage=True,
            )
        ]

        usage_chunks = [
            _parse_sse_chunk(chunk) for chunk in chunks
            if chunk != "data: [DONE]\n\n"
        ]

        final_usage_chunk = None
        for parsed in usage_chunks:
            if parsed is not None and parsed.get("choices") == [] and "usage" in parsed:
                final_usage_chunk = parsed
                break

        assert final_usage_chunk is not None, "Final usage chunk not found"
        assert final_usage_chunk["usage"]["prompt_tokens"] == 0
        assert final_usage_chunk["usage"]["completion_tokens"] == 0
        assert final_usage_chunk["usage"]["total_tokens"] == 0

    @pytest.mark.asyncio
    async def test_include_usage_false_no_usage_field(self, monkeypatch):
        async def fake_stream_chat_completion(**kwargs):
            yield b'data: {"p":"response/content","v":"Hi"}\n\n'
            yield b'data: {"p":"response/status","v":"FINISHED"}\n\n'

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.service.stream_chat_completion",
            fake_stream_chat_completion,
        )

        session = StatelessSession(chat_session_id="session-no-usage", is_initialized=False)
        chunks = [
            chunk
            async for chunk in stream_generator(
                "hi",
                "deepseek-web-chat",
                search_enabled=False,
                thinking_enabled=True,
                tools=None,
                session=session,
                include_usage=False,
            )
        ]

        for chunk in chunks:
            if chunk == "data: [DONE]\n\n":
                continue
            parsed = _parse_sse_chunk(chunk)
            if parsed is not None:
                assert "usage" not in parsed, f"Unexpected usage field: {parsed}"

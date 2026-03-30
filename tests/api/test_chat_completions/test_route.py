"""Unit tests for chat_completions route behavior."""

import json

from fastapi.testclient import TestClient
import pytest


from deepseek_web_api import app
from deepseek_web_api.api.openai.chat_completions.session_pool import StatelessSession


class _FakePool:
    def __init__(self):
        self.session = StatelessSession(chat_session_id="session-123")
        self.release_calls = []

    async def acquire(self):
        return self.session

    async def release(self, session, error=False):
        self.release_calls.append((session, error))

    async def cleanup_idle(self):
        return 0


@pytest.fixture
def client():
    return TestClient(app)


class TestChatCompletionsRoute:
    def test_non_streaming_respects_search_enabled(self, client, monkeypatch):
        captured = {}
        fake_pool = _FakePool()

        async def fake_get_pool():
            return fake_pool

        async def fake_stream_generator(prompt, model_name, search_enabled, thinking_enabled, tools, session, stop_sequences=None, include_usage=False):
            captured["search_enabled"] = search_enabled
            yield (
                'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\n'
            )
            yield "data: [DONE]\n\n"

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.route.get_pool",
            fake_get_pool,
        )
        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.route.stream_generator",
            fake_stream_generator,
        )

        response = client.post(
            "/v1/chat/completions",
            json={
                "model": "deepseek-web-chat",
                "messages": [{"role": "user", "content": "hello"}],
                "extra_body": {"search_enabled": True},
                "stream": False,
            },
        )

        assert response.status_code == 200
        assert captured["search_enabled"] is True

    def test_non_streaming_empty_chunks_returns_502(self, client, monkeypatch):
        fake_pool = _FakePool()

        async def fake_get_pool():
            return fake_pool

        async def empty_stream_generator(prompt, model_name, search_enabled, thinking_enabled, tools, session, stop_sequences=None, include_usage=False):
            if False:
                yield None

        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.route.get_pool",
            fake_get_pool,
        )
        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.route.stream_generator",
            empty_stream_generator,
        )

        response = client.post(
            "/v1/chat/completions",
            json={
                "model": "deepseek-web-chat",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": False,
            },
        )

        assert response.status_code == 502
        assert response.json()["detail"] == "DeepSeek returned no completion chunks"

    def test_streaming_acquire_failure_returns_sse_error(self, client, monkeypatch):
        class FailingPool:
            async def acquire(self):
                raise RuntimeError("create session failed")

            async def release(self, session, error=False):
                return None

            async def cleanup_idle(self):
                return 0

        async def fake_get_pool():
            return FailingPool()

        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_tokens",
            lambda: [],
        )
        monkeypatch.setattr(
            "deepseek_web_api.api.openai.chat_completions.route.get_pool",
            fake_get_pool,
        )

        with client.stream(
            "POST",
            "/v1/chat/completions",
            json={
                "model": "deepseek-web-chat",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": True,
            },
        ) as response:
            body = response.read().decode("utf-8")

        assert response.status_code == 200
        parts = [part for part in body.split("\n\n") if part]
        error_payload = json.loads(parts[0][6:])
        assert error_payload["error"]["message"].startswith("Failed to create DeepSeek session")
        assert parts[-1] == "data: [DONE]"


class TestStopFieldNormalization:
    """Tests for stop field normalization in ChatCompletionRequest."""

    def test_stop_single_string_accepted(self):
        """stop as single string should be accepted by model."""
        from deepseek_web_api.api.openai.chat_completions.route import ChatCompletionRequest

        req = ChatCompletionRequest(
            messages=[{"role": "user", "content": "hi"}],
            stop="---"
        )
        assert req.stop == "---"

    def test_stop_list_pass_through_unchanged(self):
        """stop as list should pass through unchanged."""
        from deepseek_web_api.api.openai.chat_completions.route import ChatCompletionRequest

        req = ChatCompletionRequest(
            messages=[{"role": "user", "content": "hi"}],
            stop=["\n", "---"]
        )
        assert req.stop == ["\n", "---"]

    def test_stop_none(self):
        """stop as None should remain None."""
        from deepseek_web_api.api.openai.chat_completions.route import ChatCompletionRequest

        req = ChatCompletionRequest(
            messages=[{"role": "user", "content": "hi"}],
            stop=None
        )
        assert req.stop is None


class TestResponseFormatField:
    """Tests for response_format field in ChatCompletionRequest."""

    def test_response_format_field_accepted(self):
        """response_format field should be accepted in request."""
        from deepseek_web_api.api.openai.chat_completions.route import ChatCompletionRequest

        req = ChatCompletionRequest(
            messages=[{"role": "user", "content": "hi"}],
            response_format={"type": "json_object"}
        )
        assert req.response_format == {"type": "json_object"}

    def test_response_format_json_schema(self):
        """response_format with json_schema type should be accepted."""
        from deepseek_web_api.api.openai.chat_completions.route import ChatCompletionRequest

        schema = {
            "type": "json_schema",
            "json_schema": {
                "name": "User",
                "schema": {"type": "object", "properties": {"name": {"type": "string"}}}
            }
        }
        req = ChatCompletionRequest(
            messages=[{"role": "user", "content": "hi"}],
            response_format=schema
        )
        assert req.response_format == schema

    def test_response_format_none(self):
        """response_format as None should remain None."""
        from deepseek_web_api.api.openai.chat_completions.route import ChatCompletionRequest

        req = ChatCompletionRequest(
            messages=[{"role": "user", "content": "hi"}],
            response_format=None
        )
        assert req.response_format is None

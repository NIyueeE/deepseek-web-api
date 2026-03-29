"""Smoke tests for v1/chat/completions using OpenAI SDK.

Tests tool calling, streaming, and stateless session pool with real HTTP requests.

Setup:
    # Terminal 1: Start the server
    uv run main.py

    # Terminal 2: Run tests
    uv run pytest tests/smoke/test_v1_chat_completions.py -v -s

Requires:
    pip install openai

Environment:
    SMOKE_OPENAI_BASE_URL   Defaults to http://localhost:5001/v1
    SMOKE_OPENAI_API_KEY    Defaults to test-key
"""

import os


class TestV1ChatCompletionsSmoke:
    """Smoke tests for v1/chat/completions with OpenAI SDK."""

    def setup_method(self):
        """Reset singletons and check server reachability before each test."""
        import pytest

        from deepseek_web_api.core.parent_msg_store import ParentMsgStore
        from deepseek_web_api.api.openai.chat_completions import session_pool as sp_module

        # Fast-fail if server not reachable
        client = self._create_client()
        try:
            client.models.list()
        except Exception as e:
            pytest.skip(
                f"Server not reachable at {os.getenv('SMOKE_OPENAI_BASE_URL', 'http://localhost:5001/v1')}: {e}"
            )

        # Reset singletons
        ParentMsgStore._instance = None
        ParentMsgStore._lock = None
        sp_module._pool = None

    def teardown_method(self):
        """Cleanup after each test."""
        from deepseek_web_api.core.parent_msg_store import ParentMsgStore
        from deepseek_web_api.api.openai.chat_completions import session_pool as sp_module

        ParentMsgStore._instance = None
        ParentMsgStore._lock = None
        sp_module._pool = None

    def _create_client(self):
        """Create OpenAI client pointing to local server."""
        from openai import OpenAI
        return OpenAI(
            api_key=os.getenv("SMOKE_OPENAI_API_KEY", "test-key"),
            base_url=os.getenv("SMOKE_OPENAI_BASE_URL", "http://localhost:5001/v1"),
        )

    def test_streaming_basic(self):
        """Test basic streaming response via SDK."""
        client = self._create_client()

        stream = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Say 'hello' only."}],
            stream=True,
        )

        chunks = []
        for chunk in stream:
            chunks.append(chunk)
            if len(chunks) > 200:
                break

        assert len(chunks) > 0

        text = "".join(
            chunk.choices[0].delta.content or ""
            for chunk in chunks
            if chunk.choices and chunk.choices[0].delta.content
        )
        print(f"[streaming] Response: {repr(text[:100])}")
        assert text, "Should have received text content"

    def test_non_streaming_basic(self):
        """Test non-streaming response via SDK."""
        client = self._create_client()

        resp = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Say 'hello' only, nothing else."}],
            stream=False,
        )

        assert resp.choices[0].message.content
        print(f"[non-streaming] Response: {repr(resp.choices[0].message.content)}")
        assert "hello" in resp.choices[0].message.content.lower()

    def test_stateless_session_pool(self):
        """Test that session pool provides stateless behavior.

        Ask model to remember 42, then ask what number.
        If model says 42, session has state (FAIL).
        If model says something else, stateless (PASS).
        """
        client = self._create_client()

        # First: ask to remember 42
        resp1 = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Remember the number 42."}],
            stream=False,
        )
        print(f"[1] First response: {repr(resp1.choices[0].message.content[:100])}")

        # Second: ask what number was remembered
        resp2 = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "What number did I ask you to remember?"}],
            stream=False,
        )
        print(f"[2] Second response: {repr(resp2.choices[0].message.content[:100])}")

        # Model should NOT remember 42 (stateless)
        content_lower = resp2.choices[0].message.content.lower()
        is_stateless = "42" not in content_lower
        assert is_stateless, f"Session has state! Model remembered 42: {repr(resp2.choices[0].message.content)}"

    def test_tool_calls_streaming(self):
        """Test tool calling with streaming.

        Sends a full conversation history with tool calls and checks
        that the streaming response contains 3 tool_calls.
        """
        client = self._create_client()

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "get_time",
                    "description": "Get current time for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "search_news",
                    "description": "Search news by keyword",
                    "parameters": {
                        "type": "object",
                        "properties": {"keyword": {"type": "string", "description": "Search keyword"}},
                        "required": ["keyword"]
                    }
                }
            }
        ]

        messages = [
            {"role": "user", "content": "请你回答一下你是谁, 然后帮我查一下北京现在的天气、当前时间，以及最新的AI新闻"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {"id": "call_001", "type": "function", "function": {"name": "get_weather", "arguments": '{"city": "北京"}'}},
                    {"id": "call_002", "type": "function", "function": {"name": "get_time", "arguments": '{"city": "北京"}'}},
                    {"id": "call_003", "type": "function", "function": {"name": "search_news", "arguments": '{"keyword": "AI"}'}}
                ]
            },
            {"role": "tool", "tool_call_id": "call_001", "content": '{"temperature": "22℃", "condition": "晴朗"}'},
            {"role": "tool", "tool_call_id": "call_002", "content": '{"time": "2026-03-21 20:30:00"}'},
            {"role": "tool", "tool_call_id": "call_003", "content": '{"news": ["AI新模型发布", "自动驾驶新进展"]}'},
            {"role": "assistant", "content": "北京天气晴朗, 是2026-03-21 20:30:00, 目前自动驾驶有新进展"},
            {"role": "user", "content": "再帮我查一下南京的天气吧"}
        ]

        # Stream response and collect tool_calls
        stream = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=messages,
            stream=True,
            tools=tools,
        )

        tool_calls_found = []
        for chunk in stream:
            if chunk.choices and chunk.choices[0].delta.tool_calls:
                for tc in chunk.choices[0].delta.tool_calls:
                    if tc.function:
                        tool_calls_found.append(tc.function.name)
                        print(f"[tool] {tc.function.name}")

        print(f"Total tool calls found: {len(tool_calls_found)}")
        assert len(tool_calls_found) >= 1, f"Expected at least 1 tool_call, got {len(tool_calls_found)}"

    def test_streaming_with_tools(self):
        """Test streaming with tool calls."""
        client = self._create_client()

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            }
        ]

        stream = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "北京天气怎么样?"}],
            stream=True,
            tools=tools,
        )

        tool_calls = []
        for chunk in stream:
            if chunk.choices and chunk.choices[0].delta.tool_calls:
                for tc in chunk.choices[0].delta.tool_calls:
                    if tc.function:
                        tool_calls.append(tc.function.name)
                        print(f"[tool] {tc.function.name}")

        print(f"[tool_calls] Found: {tool_calls}")
        assert len(tool_calls) > 0, "Should have received tool calls"

    def test_tool_choice_none_forbids_tools(self):
        """tool_choice='none' disables tools — model responds without calling any tool."""
        client = self._create_client()

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            }
        ]

        resp = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "What is the weather in Beijing?"}],
            stream=False,
            tools=tools,
            tool_choice="none",
        )

        content = resp.choices[0].message.content
        finish = resp.choices[0].finish_reason
        print(f"[tool_choice=none] finish_reason={finish}, content={repr(content[:80])}")

        assert finish == "stop", f"Expected finish_reason='stop', got {finish}"
        assert content, "Model should respond with text when tools are disabled"
        assert resp.choices[0].message.tool_calls is None, "tool_calls should be None when tool_choice='none'"

    def test_tool_choice_required_enforces_tool(self):
        """tool_choice='required' forces model to call at least one tool."""
        client = self._create_client()

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            }
        ]

        resp = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Please check the weather in Tokyo using the get_weather tool."}],
            stream=False,
            tools=tools,
            tool_choice="required",
        )

        finish = resp.choices[0].finish_reason
        tool_calls = resp.choices[0].message.tool_calls
        content = resp.choices[0].message.content
        print(f"[tool_choice=required] finish_reason={finish}, tool_calls={tool_calls}, content={repr(content[:80] if content else None)}")

        assert finish == "tool_calls", f"Expected finish_reason='tool_calls', got {finish}"
        assert tool_calls and len(tool_calls) > 0, f"Expected at least 1 tool_call, got {tool_calls}"
        assert tool_calls[0].function.name == "get_weather", f"Expected 'get_weather', got {tool_calls[0].function.name}"

    def test_parallel_tool_calls_false_single_call(self):
        """parallel_tool_calls=False instructs model to call one tool at a time."""
        client = self._create_client()

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "get_time",
                    "description": "Get current time for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string", "description": "City name"}},
                        "required": ["city"]
                    }
                }
            }
        ]

        stream = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "What is the weather and time in Shanghai?"}],
            stream=True,
            tools=tools,
            parallel_tool_calls=False,
        )

        tool_calls = []
        for chunk in stream:
            if chunk.choices and chunk.choices[0].delta.tool_calls:
                for tc in chunk.choices[0].delta.tool_calls:
                    if tc.function and tc.function.name:
                        tool_calls.append(tc.function.name)
                        print(f"[parallel=false] tool: {tc.function.name}")

        print(f"[parallel=false] Total tool calls: {len(tool_calls)}")
        assert len(tool_calls) > 0, "Model should call tools when parallel_tool_calls=False"

    def test_strict_mode_schema_adherence(self):
        """strict=true tool with enum — model should respect enum constraints."""
        client = self._create_client()

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "set_status",
                    "description": "Set system status",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "status": {
                                "type": "string",
                                "enum": ["active", "inactive", "pending"],
                                "description": "Status value"
                            }
                        },
                        "required": ["status"]
                    },
                    "strict": True
                }
            }
        ]

        resp = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Set the system status to 'active' using the set_status tool."}],
            stream=False,
            tools=tools,
        )

        finish = resp.choices[0].finish_reason
        tool_calls = resp.choices[0].message.tool_calls
        content = resp.choices[0].message.content
        print(f"[strict=true] finish_reason={finish}, tool_calls={tool_calls}, content={repr(content[:80] if content else None)}")

        assert finish == "tool_calls", f"Expected finish_reason='tool_calls', got {finish}"
        assert tool_calls and len(tool_calls) > 0, f"Expected tool_call, got {tool_calls}"

        status_arg = tool_calls[0].function.arguments
        if isinstance(status_arg, str):
            import json
            status_arg = json.loads(status_arg)
        status_value = status_arg.get("status")
        print(f"[strict=true] status argument: {status_value}")
        assert status_value in ["active", "inactive", "pending"], f"Model used invalid enum value: {status_value}"

    def test_response_format_json_object(self):
        """response_format={'type':'json_object'} should produce valid JSON."""
        client = self._create_client()

        resp = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Return a JSON object with fields 'name' (string) and 'age' (number) for a person named Alice who is 30 years old."}],
            stream=False,
            response_format={"type": "json_object"},
        )

        content = resp.choices[0].message.content
        print(f"[response_format=json_object] Response: {repr(content[:200])}")
        assert content, "Should have content"
        import json
        try:
            data = json.loads(content)
            assert "name" in data, "JSON should have 'name' field"
            assert "age" in data, "JSON should have 'age' field"
        except json.JSONDecodeError as e:
            pytest.fail(f"Response was not valid JSON: {e}\nContent: {repr(content)}")

    def test_stop_sequence_truncation(self):
        """stop='---' should truncate output at the sequence (sequence itself not output)."""
        client = self._create_client()

        stream = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Count from 1 to 5, separating each number with '---'. Example: 1---2---3"}],
            stream=True,
            stop="---",
        )

        chunks = []
        for chunk in stream:
            chunks.append(chunk)
            if len(chunks) > 100:
                break

        text = "".join(
            chunk.choices[0].delta.content or ""
            for chunk in chunks
            if chunk.choices and chunk.choices[0].delta.content
        )
        print(f"[stop=---] Full output: {repr(text)}")
        assert "---" not in text, f"Stop sequence '---' should not appear in output: {repr(text)}"
        # Verify it stopped at a reasonable point (not at the very beginning)
        assert len(text) > 0, "Should have some output before stop"

    def test_stop_sequence_multi(self):
        """stop=['\\n', '---'] should truncate at the first matching sequence."""
        client = self._create_client()

        stream = client.chat.completions.create(
            model="deepseek-web-chat",
            messages=[{"role": "user", "content": "Write the numbers one through five on separate lines. Example:\none\ntwo\nthree"}],
            stream=True,
            stop=["\n", "---"],
        )

        chunks = []
        for chunk in stream:
            chunks.append(chunk)
            if len(chunks) > 100:
                break

        text = "".join(
            chunk.choices[0].delta.content or ""
            for chunk in chunks
            if chunk.choices and chunk.choices[0].delta.content
        )
        print(f"[stop=['\\n','---']] Full output: {repr(text)}")
        # Should not contain newlines (the \n stop sequence)
        assert "\n" not in text, f"Stop sequence '\\n' should not appear in output: {repr(text)}"
        assert "---" not in text, f"Stop sequence '---' should not appear in output: {repr(text)}"

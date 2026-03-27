"""Smoke tests for v0 API using real DeepSeek API.

These tests use real credentials and make actual API calls to test
the complete v0_service layer end-to-end.

Run with: pytest tests/smoke/test_v0_api.py -v -s
"""


import pytest



class TestV0ApiSmoke:
    """Smoke tests for v0 API service layer with real API calls."""

    @pytest.fixture(autouse=True)
    def reset_store(self):
        """Reset ParentMsgStore before each test."""
        from deepseek_web_api.core.parent_msg_store import ParentMsgStore

        ParentMsgStore._instance = None
        ParentMsgStore._lock = None
        yield
        ParentMsgStore._instance = None
        ParentMsgStore._lock = None

    def _parse_sse_text(self, chunks: list[bytes]) -> str:
        """Parse SSE chunks to extract text content."""
        import json
        text_parts = []
        for chunk in chunks:
            try:
                decoded = chunk.decode("utf-8")
                for line in decoded.split("\n"):
                    line = line.strip()
                    if not line.startswith("data:"):
                        continue
                    data_str = line[5:].strip()
                    if not data_str:
                        continue
                    data = json.loads(data_str)
                    # Streaming format: {"o": "APPEND", "v": "text"}
                    if data.get("o") == "APPEND":
                        text_parts.append(data.get("v", ""))
                    # Path-based update: {"p": "response/content", "v": "text"}
                    elif data.get("p") == "response/content":
                        text_parts.append(data.get("v", ""))
                    # Full response in update_session: {"v": {"response": {"content": "text"}}}
                    elif "v" in data and isinstance(data["v"], dict):
                        response = data["v"].get("response", {})
                        if isinstance(response, dict) and "content" in response:
                            content = response.get("content", "")
                            if content:
                                text_parts.append(content)
            except Exception:
                continue
        return "".join(text_parts)

    @pytest.mark.asyncio
    async def test_full_conversation_flow(self):
        """Test complete conversation flow end-to-end with all v0 endpoints."""
        import json

        from deepseek_web_api.api.v0_service import (
            create_session,
            delete_session,
            get_history_messages,
            stream_chat_completion,
            upload_file,
            fetch_files,
        )

        print("\n=== Starting full conversation flow ===")

        # 1. Create session
        session_id, _ = await create_session()
        print(f"[1] Session created: {session_id}")

        # 2. Upload file with this session
        test_content = b"# Test File\n\nThis is a test."
        upload_resp = await upload_file(
            test_content,
            "test.txt",
            "text/plain"
        )
        print(f"[2] File uploaded: status={upload_resp.status_code}")
        upload_data = json.loads(upload_resp.body)
        file_id = upload_data.get("data", {}).get("biz_data", {}).get("id")
        print(f"[2] File ID: {file_id}")

        # 3. Fetch file status
        if file_id:
            fetch_resp = await fetch_files(file_id)
            print(f"[3] File status fetched: status={fetch_resp.status_code}")

        # 4. Delete session
        await delete_session(session_id)
        print("[4] Session deleted")

        # 5. Multi-turn conversation (new session)
        session_id2, _ = await create_session()
        print(f"[5] New session for multi-turn: {session_id2}")

        # First message - ask for "42"
        chunks1 = []
        async for chunk in stream_chat_completion(
            "Reply with only the number 42, nothing else.",
            chat_session_id=session_id2,
            search_enabled=False,
            thinking_enabled=False,
        ):
            chunks1.append(chunk)
            if len(chunks1) > 200:
                break

        text1 = self._parse_sse_text(chunks1)
        print(f"[5a] First response: {repr(text1)}")
        assert "42" in text1, f"First should contain '42', got: {repr(text1)}"

        # Second message - refer to previous
        chunks2 = []
        async for chunk in stream_chat_completion(
            "Reply with only the number from my previous message, nothing else.",
            chat_session_id=session_id2,
            search_enabled=False,
            thinking_enabled=False,
        ):
            chunks2.append(chunk)
            if len(chunks2) > 200:
                break

        text2 = self._parse_sse_text(chunks2)
        print(f"[5b] Second response: {repr(text2)}")
        assert "42" in text2, f"Second should contain '42', got: {repr(text2)}"
        print("[5] Multi-turn conversation verified: both responses contain '42'")

        # 6. Get history and verify both "42" messages
        history_resp = await get_history_messages(session_id2, offset=0, limit=10)
        print(f"[6] History retrieved: status={history_resp.status_code}")
        history_data = json.loads(history_resp.body)
        messages = history_data.get("data", {}).get("biz_data", {}).get("chat_messages", [])
        print(f"[6] Total messages in history: {len(messages)}")

        # Verify messages contain "42"
        found_42_count = 0
        for msg in messages:
            content = msg.get("content", "")
            if "42" in content:
                found_42_count += 1
                print(f"[6] Found '42' in message: {content[:50]}")
        assert found_42_count >= 2, f"Expected at least 2 messages with '42', found {found_42_count}"
        print(f"[6] Verified: {found_42_count} messages contain '42'")

        # 7. Delete session
        await delete_session(session_id2)
        print(f"[7] Session {session_id2} deleted")

        print("=== Full flow completed successfully ===")

    @pytest.mark.asyncio
    async def test_message_stateless_conversation(self):
        """Test message endpoint with fixed message_id=1 for stateless multi-turn.

        Flow: create_session → completion (creates message) → edit_message (edits message_id=1)
        """
        from deepseek_web_api.api.v0_service import (
            create_session,
            delete_session,
            stream_chat_completion,
            stream_edit_message,
        )

        print("\n=== Testing message stateless endpoint ===")

        # 1. Create session
        session_id, _ = await create_session()
        print(f"[1] Session created: {session_id}")

        # 2. First completion - creates first message
        chunks1 = []
        async for chunk in stream_chat_completion(
            "Reply with only the number 42, nothing else.",
            chat_session_id=session_id,
            search_enabled=False,
            thinking_enabled=False,
        ):
            chunks1.append(chunk)
            if len(chunks1) > 200:
                break

        text1 = self._parse_sse_text(chunks1)
        print(f"[2] First response (completion): {repr(text1)}")
        assert "42" in text1, f"First should contain '42', got: {repr(text1)}"

        # 3-5. Repeat edit_message 3 times
        # Each time model should NOT remember any previous context
        expected_words = ["42", "forty-two", "fourty two"]
        for i in range(3):
            chunks2 = []
            async for chunk in stream_edit_message(
                "Reply with a random number under 100 (not 42), only the number.",
                chat_session_id=session_id,
                search_enabled=False,
                thinking_enabled=False,
            ):
                chunks2.append(chunk)
                if len(chunks2) > 200:
                    break

            text2 = self._parse_sse_text(chunks2)
            print(f"[{3+i}] Edit response {i+1}: {repr(text2)}")

            # Model should NOT answer "42" (or variants) - proves stateless
            is_stateless = not any(word in text2.lower() for word in expected_words)
            assert is_stateless, f"Model remembered! Response: {repr(text2)}"
            print(f"[{3+i}] Verified stateless: model did not say '42'")

        await delete_session(session_id)
        print("[4] Session deleted")
        print("=== Message stateless test completed ===")

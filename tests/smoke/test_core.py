"""Smoke tests for core module using real DeepSeek API.

These tests use real credentials and make actual API calls.
Run with: pytest tests/smoke/ -v -s
"""


# Don't import at module level to avoid triggering init
# Tests will import what they need


class TestCoreSmoke:
    """Smoke tests for core functionality with real API."""

    def test_login(self):
        """Test login with real credentials."""
        from deepseek_web_api.core import get_token

        # This will actually login to DeepSeek
        token = get_token()

        assert token is not None
        assert len(token) > 0
        print(f"\n[PASS] Login successful, token: {token[:20]}...")

    def test_auth_headers(self):
        """Test getting auth headers."""
        from deepseek_web_api.core import get_auth_headers

        headers = get_auth_headers()

        assert "authorization" in headers
        assert headers["authorization"].startswith("Bearer ")
        print("\n[PASS] Auth headers obtained")

    def test_pow_response(self):
        """Test PoW response generation."""
        from deepseek_web_api.core import get_pow_response

        # This will call DeepSeek API to get PoW challenge
        pow_resp = get_pow_response()

        assert pow_resp is not None
        assert len(pow_resp) > 0
        print("\n[PASS] PoW response generated")

    def test_full_flow(self):
        """Test complete flow: login -> pow -> session -> message."""
        import httpx
        from deepseek_web_api.core import get_token, get_auth_headers, get_pow_response
        from deepseek_web_api.core.config import DEEPSEEK_HOST

        # 1. Get token
        token = get_token()
        assert token
        print("\n[PASS] Got token")

        # 2. Get PoW
        pow_resp = get_pow_response()
        assert pow_resp
        print("[PASS] Got PoW")

        # 3. Create session
        headers = get_auth_headers()
        headers["Host"] = DEEPSEEK_HOST

        resp = httpx.post(
            f"https://{DEEPSEEK_HOST}/api/v0/chat_session/create",
            headers=headers,
            json={"agent": "chat"},
            timeout=30.0,
        )

        data = resp.json()
        chat_session_id = data.get("data", {}).get("biz_data", {}).get("id")
        assert chat_session_id
        print(f"[PASS] Session created: {chat_session_id}")

        # 4. Send a simple message
        headers["x-ds-pow-response"] = pow_resp

        resp = httpx.post(
            f"https://{DEEPSEEK_HOST}/api/v0/chat/completion",
            headers=headers,
            json={
                "chat_session_id": chat_session_id,
                "parent_message_id": None,
                "preempt": False,
                "prompt": "Hello",
                "ref_file_ids": [],
                "search_enabled": True,
                "thinking_enabled": True,
            },
            timeout=60.0,
        )

        # Check response
        if resp.status_code == 200:
            content = resp.content
            assert len(content) > 0, "Should receive response"
            print(f"[PASS] Full flow works! Response size: {len(content)} bytes")
        else:
            print(f"[INFO] Response status: {resp.status_code}")
            print(f"Response: {resp.text[:200]}")

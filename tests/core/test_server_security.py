"""Tests for startup security warnings."""

import pytest

from deepseek_web_api.core import server_security


class TestServerSecurity:
    def test_is_loopback_host(self):
        assert server_security.is_loopback_host("127.0.0.1") is True
        assert server_security.is_loopback_host("localhost") is True
        assert server_security.is_loopback_host("[::1]") is True
        assert server_security.is_loopback_host("0.0.0.0") is False

    def test_collect_startup_security_warnings_for_open_defaults(self, monkeypatch):
        monkeypatch.setattr(server_security, "get_server_host", lambda: "127.0.0.1")
        monkeypatch.setattr(server_security, "get_auth_required", lambda: False)
        monkeypatch.setattr(server_security, "has_effective_auth_tokens", lambda: False)
        monkeypatch.setattr(server_security, "get_cors_origins", lambda: ["*"])

        warnings = server_security.collect_startup_security_warnings()

        assert any("Local API auth is disabled" in warning for warning in warnings)
        assert any("CORS allows all origins" in warning for warning in warnings)
        assert not any("not loopback" in warning for warning in warnings)

    def test_collect_startup_security_warnings_for_remote_exposure(self, monkeypatch):
        monkeypatch.setattr(server_security, "get_server_host", lambda: "0.0.0.0")
        monkeypatch.setattr(server_security, "get_auth_required", lambda: False)
        monkeypatch.setattr(server_security, "has_effective_auth_tokens", lambda: False)
        monkeypatch.setattr(server_security, "get_cors_origins", lambda: ["https://app.example.com"])

        warnings = server_security.collect_startup_security_warnings()

        assert any("not loopback" in warning for warning in warnings)
        assert any("unsafe" in warning for warning in warnings)

    def test_collect_startup_security_warnings_for_hardened_config(self, monkeypatch):
        monkeypatch.setattr(server_security, "get_server_host", lambda: "127.0.0.1")
        monkeypatch.setattr(server_security, "get_auth_required", lambda: False)
        monkeypatch.setattr(server_security, "has_effective_auth_tokens", lambda: True)
        monkeypatch.setattr(server_security, "get_cors_origins", lambda: ["https://app.example.com"])

        warnings = server_security.collect_startup_security_warnings()

        assert warnings == []

    def test_collect_startup_security_warnings_for_required_without_tokens(self, monkeypatch):
        monkeypatch.setattr(server_security, "get_server_host", lambda: "127.0.0.1")
        monkeypatch.setattr(server_security, "get_auth_required", lambda: True)
        monkeypatch.setattr(server_security, "has_effective_auth_tokens", lambda: False)
        monkeypatch.setattr(server_security, "get_cors_origins", lambda: ["https://app.example.com"])

        warnings = server_security.collect_startup_security_warnings()

        assert any("will reject every request with 401" in warning for warning in warnings)
        assert not any("unsafe" in warning for warning in warnings)

    @pytest.mark.parametrize(
        "summary",
        [
            "Auth mode: legacy single-token compatibility mode; 1 enabled token(s); required=False.",
            "Auth mode: formal auth.tokens mode; 1 enabled token(s); required=True.",
            "Auth mode: mixed compatibility mode; 2 enabled token(s); required=False.",
        ],
    )
    def test_log_startup_security_warnings_logs_auth_mode_summary(self, monkeypatch, summary):
        captured = []

        monkeypatch.setattr(server_security, "get_auth_mode_summary", lambda: summary)
        monkeypatch.setattr(server_security, "collect_startup_security_warnings", lambda: [])
        monkeypatch.setattr(server_security.logger, "warning", captured.append)
        monkeypatch.setattr(server_security.logger, "info", captured.append)

        server_security.log_startup_security_warnings()

        assert any(summary in message for message in captured)

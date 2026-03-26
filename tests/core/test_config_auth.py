"""Tests for auth-related config normalization helpers."""

import sys

sys.path.insert(0, "src")

from deepseek_web_api.core import config


class TestAuthConfig:
    def test_legacy_api_key_is_normalized_as_compat_token(self, monkeypatch):
        monkeypatch.delenv("DEEPSEEK_WEB_API_KEY", raising=False)
        monkeypatch.setattr(config, "CONFIG", {"server": {"api_key": "legacy-secret"}})

        assert config.get_local_api_key() == "legacy-secret"
        assert config.get_auth_tokens() == [
            {"name": "legacy-api-key", "token": "legacy-secret", "enabled": True}
        ]
        assert config.get_enabled_auth_tokens() == ["legacy-secret"]
        assert config.has_effective_auth_tokens() is True
        assert config.get_auth_mode_name() == "legacy single-token compatibility mode"

    def test_env_api_key_overrides_legacy_server_api_key(self, monkeypatch):
        monkeypatch.setenv("DEEPSEEK_WEB_API_KEY", "env-secret")
        monkeypatch.setattr(config, "CONFIG", {"server": {"api_key": "legacy-secret"}})

        assert config.get_local_api_key() == "env-secret"
        assert config.get_enabled_auth_tokens() == ["env-secret"]

    def test_formal_auth_tokens_default_enabled_and_set_required(self, monkeypatch):
        monkeypatch.delenv("DEEPSEEK_WEB_API_KEY", raising=False)
        monkeypatch.setattr(
            config,
            "CONFIG",
            {
                "auth": {
                    "required": True,
                    "tokens": [
                        {"name": "prod-gateway", "token": "prod-secret"},
                        {"token": "fallback-secret"},
                    ],
                }
            },
        )

        assert config.get_auth_required() is True
        assert config.get_auth_tokens() == [
            {"name": "prod-gateway", "token": "prod-secret", "enabled": True},
            {"name": "auth-token-2", "token": "fallback-secret", "enabled": True},
        ]
        assert config.get_enabled_auth_tokens() == ["prod-secret", "fallback-secret"]
        assert config.get_auth_mode_name() == "formal auth.tokens mode"

    def test_mixed_auth_sources_merge_and_skip_disabled_tokens(self, monkeypatch):
        monkeypatch.delenv("DEEPSEEK_WEB_API_KEY", raising=False)
        monkeypatch.setattr(
            config,
            "CONFIG",
            {
                "server": {"api_key": "legacy-secret"},
                "auth": {
                    "tokens": [
                        {"name": "primary", "token": "primary-secret", "enabled": True},
                        {"name": "disabled", "token": "disabled-secret", "enabled": False},
                    ]
                },
            },
        )

        assert config.get_auth_tokens() == [
            {"name": "legacy-api-key", "token": "legacy-secret", "enabled": True},
            {"name": "primary", "token": "primary-secret", "enabled": True},
            {"name": "disabled", "token": "disabled-secret", "enabled": False},
        ]
        assert config.get_enabled_auth_tokens() == ["legacy-secret", "primary-secret"]
        assert config.get_auth_mode_name() == "mixed compatibility mode"

    def test_required_defaults_false_without_effective_tokens(self, monkeypatch):
        monkeypatch.delenv("DEEPSEEK_WEB_API_KEY", raising=False)
        monkeypatch.setattr(
            config,
            "CONFIG",
            {
                "auth": {
                    "required": False,
                    "tokens": [
                        {"name": "disabled", "token": "disabled-secret", "enabled": False},
                        {"name": "blank", "token": "   ", "enabled": True},
                    ]
                }
            },
        )

        assert config.get_auth_required() is False
        assert config.get_auth_tokens() == [
            {"name": "disabled", "token": "disabled-secret", "enabled": False}
        ]
        assert config.get_enabled_auth_tokens() == []
        assert config.has_effective_auth_tokens() is False
        assert config.get_auth_mode_name() == "formal auth.tokens mode"

    def test_auth_mode_summary_reports_enabled_count_and_required(self, monkeypatch):
        monkeypatch.delenv("DEEPSEEK_WEB_API_KEY", raising=False)
        monkeypatch.setattr(
            config,
            "CONFIG",
            {
                "server": {"api_key": "legacy-secret"},
                "auth": {
                    "required": True,
                    "tokens": [
                        {"name": "primary", "token": "primary-secret", "enabled": True},
                    ],
                },
            },
        )

        assert config.get_auth_mode_summary() == (
            "Auth mode: mixed compatibility mode; 2 enabled token(s); required=True."
        )

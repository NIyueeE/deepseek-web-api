"""Tests for API route-level middleware and auth behavior."""


from fastapi.testclient import TestClient
import pytest


from deepseek_web_api import app
from deepseek_web_api.api import routes


@pytest.fixture
def client():
    return TestClient(app)


class TestLocalApiAuthMiddleware:
    def test_root_endpoint_is_not_protected(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: True,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: ["secret-token"],
        )

        response = client.get("/")

        assert response.status_code == 200
        assert response.json()["status"] == "ok"

    def test_v1_models_requires_auth_when_token_configured(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: False,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: ["secret-token"],
        )

        response = client.get("/v1/models")

        assert response.status_code == 401
        assert response.json()["detail"] == "Invalid or missing local API key"

    def test_v1_models_accepts_bearer_token(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: False,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: ["secret-token"],
        )

        response = client.get(
            "/v1/models",
            headers={"Authorization": "Bearer secret-token"},
        )

        assert response.status_code == 200
        assert response.json()["object"] == "list"

    def test_v1_models_accepts_x_api_key(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: False,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: ["secret-token"],
        )

        response = client.get(
            "/v1/models",
            headers={"X-API-Key": "secret-token"},
        )

        assert response.status_code == 200
        assert response.json()["object"] == "list"

    def test_v1_models_remains_open_when_auth_not_required_and_no_tokens(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: False,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: [],
        )

        response = client.get("/v1/models")

        assert response.status_code == 200
        assert response.json()["object"] == "list"

    def test_v1_models_rejects_disabled_or_unknown_token(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: False,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: ["enabled-token"],
        )

        response = client.get(
            "/v1/models",
            headers={"Authorization": "Bearer disabled-token"},
        )

        assert response.status_code == 401
        assert response.json()["detail"] == "Invalid or missing local API key"

    def test_v1_models_accepts_any_enabled_token_in_mixed_mode(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: False,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: ["legacy-secret", "formal-secret"],
        )

        legacy_response = client.get(
            "/v1/models",
            headers={"Authorization": "Bearer legacy-secret"},
        )
        formal_response = client.get(
            "/v1/models",
            headers={"X-API-Key": "formal-secret"},
        )

        assert legacy_response.status_code == 200
        assert formal_response.status_code == 200

    def test_v1_models_requires_auth_when_required_without_tokens(self, client, monkeypatch):
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_auth_required",
            lambda: True,
        )
        monkeypatch.setattr(
            "deepseek_web_api.core.local_api_auth.get_enabled_auth_tokens",
            lambda: [],
        )

        response = client.get("/v1/models")

        assert response.status_code == 401
        assert response.json()["detail"] == "Invalid or missing local API key"


class TestCorsConfiguration:
    def test_get_cors_middleware_options_uses_config_helpers(self, monkeypatch):
        monkeypatch.setattr(routes, "get_cors_origins", lambda: ["http://localhost:3000"])
        monkeypatch.setattr(routes, "get_cors_allow_credentials", lambda: True)
        monkeypatch.setattr(routes, "get_cors_allow_methods", lambda: ["GET", "POST"])
        monkeypatch.setattr(routes, "get_cors_allow_headers", lambda: ["Authorization", "Content-Type"])
        monkeypatch.setattr(routes, "get_cors_origin_regex", lambda: "^https://.*\\.example\\.com$")

        options = routes.get_cors_middleware_options()

        assert options == {
            "allow_origins": ["http://localhost:3000"],
            "allow_credentials": True,
            "allow_methods": ["GET", "POST"],
            "allow_headers": ["Authorization", "Content-Type"],
            "allow_origin_regex": "^https://.*\\.example\\.com$",
        }

    def test_get_cors_middleware_options_omits_regex_when_empty(self, monkeypatch):
        monkeypatch.setattr(routes, "get_cors_origins", lambda: ["*"])
        monkeypatch.setattr(routes, "get_cors_allow_credentials", lambda: False)
        monkeypatch.setattr(routes, "get_cors_allow_methods", lambda: ["*"])
        monkeypatch.setattr(routes, "get_cors_allow_headers", lambda: ["*"])
        monkeypatch.setattr(routes, "get_cors_origin_regex", lambda: None)

        options = routes.get_cors_middleware_options()

        assert options == {
            "allow_origins": ["*"],
            "allow_credentials": False,
            "allow_methods": ["*"],
            "allow_headers": ["*"],
        }

"""Configuration and constants for DeepSeek API."""

import json
import logging
import os
import pathlib
from typing import Any

try:
    import tomllib as toml
except ImportError:
    import tomli as toml

logger = logging.getLogger(__name__)

# ----------------------------------------------------------------------
# (1) Configuration file path and load/save functions
# ----------------------------------------------------------------------
CONFIG_PATH = os.getenv("CONFIG_PATH", "config.toml")


def load_config():
    """Load configuration from config.toml, return empty dict on error."""
    try:
        with open(CONFIG_PATH, "rb") as f:
            return toml.load(f)
    except Exception as e:
        logger.warning(f"[load_config] Cannot read config file: {e}")
        return {}


def save_config(cfg):
    """Write configuration back to config.toml.

    Uses tomli-w if available (Python 3.11+), otherwise falls back to json.
    """
    try:
        try:
            import tomli_w

            with open(CONFIG_PATH, "wb") as f:
                tomli_w.dump(cfg, f)
        except ImportError:
            # Fallback: write as JSON with TOML extension warning
            json_path = CONFIG_PATH.replace(".toml", ".json")
            logger.warning(
                f"[save_config] tomli-w not available, saving as JSON to {json_path}"
            )
            with open(json_path, "w", encoding="utf-8") as f:
                json.dump(cfg, f, ensure_ascii=False, indent=2)
    except Exception as e:
        logger.error(f"[save_config] Failed to write config file: {e}")


CONFIG = load_config()


def _get_server_config() -> dict:
    return CONFIG.get("server", {})


def _get_auth_config() -> dict:
    auth_cfg = CONFIG.get("auth", {})
    return auth_cfg if isinstance(auth_cfg, dict) else {}


def _get_env_or_config(env_name: str, config_key: str, default):
    env_value = os.getenv(env_name)
    if env_value is not None:
        return env_value
    return _get_server_config().get(config_key, default)


def _parse_bool(value, default: bool) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"1", "true", "yes", "on"}:
            return True
        if normalized in {"0", "false", "no", "off"}:
            return False
    return bool(value)


def _parse_csv_or_list(value, default: list[str]) -> list[str]:
    if value is None:
        return list(default)
    if isinstance(value, list):
        return [str(item).strip() for item in value if str(item).strip()]
    if isinstance(value, str):
        items = [item.strip() for item in value.split(",")]
        return [item for item in items if item]
    return list(default)


def _normalize_auth_token_entry(
    raw_entry: Any,
    *,
    fallback_name: str,
    default_enabled: bool = True,
) -> dict[str, Any] | None:
    if not isinstance(raw_entry, dict):
        return None

    token = str(raw_entry.get("token", "")).strip()
    if not token:
        return None

    name = str(raw_entry.get("name", "")).strip() or fallback_name
    enabled = _parse_bool(raw_entry.get("enabled"), default_enabled)
    return {"name": name, "token": token, "enabled": enabled}


def _get_explicit_auth_tokens() -> list[dict[str, Any]]:
    auth_cfg = _get_auth_config()
    raw_tokens = auth_cfg.get("tokens", [])
    if not isinstance(raw_tokens, list):
        return []

    tokens = []
    for index, raw_entry in enumerate(raw_tokens, start=1):
        normalized = _normalize_auth_token_entry(
            raw_entry,
            fallback_name=f"auth-token-{index}",
        )
        if normalized:
            tokens.append(normalized)
    return tokens


def _get_legacy_auth_token_entry() -> dict[str, Any] | None:
    token = get_local_api_key()
    if not token:
        return None
    return {
        "name": "legacy-api-key",
        "token": token,
        "enabled": True,
    }

# ----------------------------------------------------------------------
# (2) DeepSeek API constants
# ----------------------------------------------------------------------
DEEPSEEK_HOST = "chat.deepseek.com"
DEEPSEEK_LOGIN_URL = f"https://{DEEPSEEK_HOST}/api/v0/users/login"
DEEPSEEK_CREATE_POW_URL = f"https://{DEEPSEEK_HOST}/api/v0/chat/create_pow_challenge"

# BASE_HEADERS must be configured in config.toml under [headers]
# See config.toml.example for required fields
BASE_HEADERS = CONFIG.get("headers", {})

# HTTP request impersonation (browser signature for anti-bot)
# Can be in [browser.impersonate] or root level impersonate
DEFAULT_IMPERSONATE = CONFIG.get("browser", {}).get("impersonate") or CONFIG.get("impersonate", "")

# WASM module file path (relative to core module, or absolute)
_default_wasm = pathlib.Path(__file__).parent / "sha3_wasm_bg.7b9ca65ddd.wasm"
WASM_PATH = os.getenv("WASM_PATH", str(_default_wasm))

# Log level from config (default WARNING if not set)
_log_level_str = CONFIG.get("log_level", "WARNING").upper()
LOG_LEVEL = getattr(logging, _log_level_str, logging.WARNING)


def get_local_api_key() -> str:
    """Get the legacy compatibility API key for protecting this proxy service.

    Environment variable takes precedence over config.toml.
    """
    env_key = os.getenv("DEEPSEEK_WEB_API_KEY", "").strip()
    if env_key:
        return env_key

    server_cfg = _get_server_config()
    return str(server_cfg.get("api_key", "")).strip()


def get_auth_required() -> bool:
    """Return whether auth is explicitly required for /v0 and /v1."""
    return _parse_bool(_get_auth_config().get("required"), False)


def get_auth_tokens() -> list[dict[str, Any]]:
    """Return normalized auth token entries from legacy and formal config sources."""
    tokens = []

    legacy_token = _get_legacy_auth_token_entry()
    if legacy_token:
        tokens.append(legacy_token)

    tokens.extend(_get_explicit_auth_tokens())
    return tokens


def get_enabled_auth_tokens() -> list[str]:
    """Return enabled auth token values from all supported config sources."""
    return [entry["token"] for entry in get_auth_tokens() if entry["enabled"]]


def has_effective_auth_tokens() -> bool:
    """Return True when at least one enabled auth token is configured."""
    return bool(get_enabled_auth_tokens())


def get_auth_mode_name() -> str:
    """Describe which auth config source(s) are currently in effect."""
    has_legacy = _get_legacy_auth_token_entry() is not None
    has_explicit = bool(_get_explicit_auth_tokens())

    if has_legacy and has_explicit:
        return "mixed compatibility mode"
    if has_explicit:
        return "formal auth.tokens mode"
    if has_legacy:
        return "legacy single-token compatibility mode"
    return "anonymous mode"


def get_auth_mode_summary() -> str:
    """Return a log-safe summary of the active auth mode."""
    enabled_count = len(get_enabled_auth_tokens())
    required = get_auth_required()
    return (
        f"Auth mode: {get_auth_mode_name()}; "
        f"{enabled_count} enabled token(s); required={required}."
    )


def get_server_host() -> str:
    return str(_get_env_or_config("DEEPSEEK_WEB_HOST", "host", "127.0.0.1")).strip()


def get_server_port() -> int:
    return int(_get_env_or_config("DEEPSEEK_WEB_PORT", "port", 5001))


def get_server_reload() -> bool:
    return _parse_bool(_get_env_or_config("DEEPSEEK_WEB_RELOAD", "reload", True), True)


def get_cors_origins() -> list[str]:
    return _parse_csv_or_list(
        _get_env_or_config("DEEPSEEK_WEB_CORS_ORIGINS", "cors_origins", ["*"]),
        ["*"],
    )


def get_cors_origin_regex() -> str | None:
    value = _get_env_or_config("DEEPSEEK_WEB_CORS_ORIGIN_REGEX", "cors_origin_regex", "")
    value = str(value).strip()
    return value or None


def get_cors_allow_credentials() -> bool:
    return _parse_bool(
        _get_env_or_config("DEEPSEEK_WEB_CORS_ALLOW_CREDENTIALS", "cors_allow_credentials", False),
        False,
    )


def get_cors_allow_methods() -> list[str]:
    return _parse_csv_or_list(
        _get_env_or_config("DEEPSEEK_WEB_CORS_ALLOW_METHODS", "cors_allow_methods", ["*"]),
        ["*"],
    )


def get_cors_allow_headers() -> list[str]:
    return _parse_csv_or_list(
        _get_env_or_config("DEEPSEEK_WEB_CORS_ALLOW_HEADERS", "cors_allow_headers", ["*"]),
        ["*"],
    )

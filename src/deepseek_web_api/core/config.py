"""Configuration and constants for DeepSeek API."""

import json
import logging
import os
import pathlib

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


def get_auth_tokens() -> list[str]:
    """Return auth tokens from config. Non-empty list means auth is required."""
    auth_cfg = CONFIG.get("auth", {})
    if not isinstance(auth_cfg, dict):
        return []
    tokens = auth_cfg.get("tokens", [])
    if not isinstance(tokens, list):
        return []
    return [str(t).strip() for t in tokens if str(t).strip()]


def get_pool_size() -> int:
    """Max concurrent DeepSeek sessions in the stateless session pool."""
    return int(_get_server_config().get("pool_size", 10))


def get_pool_acquire_timeout() -> float:
    """Seconds to wait for an available session before returning 503."""
    return float(_get_server_config().get("pool_acquire_timeout", 30.0))


def get_server_host() -> str:
    return str(_get_server_config().get("host", "127.0.0.1")).strip()


def get_server_port() -> int:
    return int(_get_server_config().get("port", 5001))


def get_server_reload() -> bool:
    return _parse_bool(_get_server_config().get("reload"), True)


def get_cors_origins() -> list[str]:
    return _parse_csv_or_list(
        _get_server_config().get("cors_origins", ["*"]),
        ["*"],
    )


def get_cors_origin_regex() -> str | None:
    value = _get_server_config().get("cors_origin_regex", "")
    value = str(value).strip()
    return value or None


def get_cors_allow_credentials() -> bool:
    return _parse_bool(
        _get_server_config().get("cors_allow_credentials"),
        False,
    )


def get_cors_allow_methods() -> list[str]:
    return _parse_csv_or_list(
        _get_server_config().get("cors_allow_methods", ["*"]),
        ["*"],
    )


def get_cors_allow_headers() -> list[str]:
    return _parse_csv_or_list(
        _get_server_config().get("cors_allow_headers", ["*"]),
        ["*"],
    )

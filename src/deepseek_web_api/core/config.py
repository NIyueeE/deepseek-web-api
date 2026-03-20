"""Configuration and constants for DeepSeek API."""

import json
import logging
import os

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

# ----------------------------------------------------------------------
# (2) DeepSeek API constants
# ----------------------------------------------------------------------
DEEPSEEK_HOST = "chat.deepseek.com"
DEEPSEEK_LOGIN_URL = f"https://{DEEPSEEK_HOST}/api/v0/users/login"
DEEPSEEK_CREATE_POW_URL = f"https://{DEEPSEEK_HOST}/api/v0/chat/create_pow_challenge"

BASE_HEADERS = {
    "Host": "chat.deepseek.com",
    "User-Agent": "DeepSeek/1.0.13 Android/35",
    "Accept": "application/json",
    "Accept-Encoding": "gzip",
    "Content-Type": "application/json",
    "x-client-platform": "android",
    "x-client-version": "1.3.0-auto-resume",
    "x-client-locale": "zh_CN",
    "accept-charset": "UTF-8",
}

# WASM module file path
WASM_PATH = os.getenv("WASM_PATH", "sha3_wasm_bg.7b9ca65ddd.wasm")

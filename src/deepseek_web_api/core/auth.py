"""Authentication for single account mode."""

import logging

from curl_cffi import requests

from .config import BASE_HEADERS, CONFIG, DEEPSEEK_LOGIN_URL, save_config

logger = logging.getLogger(__name__)

# Global single account
_account = None
_token = None


def init_single_account():
    """Initialize single account from config."""
    global _account, _token
    _account = CONFIG.get("account")
    if not _account:
        raise ValueError("No account configured")
    _token = _account.get("token", "")
    if not _token:
        _token = login()


def login() -> str:
    """Login and get new token."""
    global _account, _token
    email = _account.get("email", "").strip()
    mobile = _account.get("mobile", "").strip()
    password = _account.get("password", "").strip()

    if not password or (not email and not mobile):
        raise ValueError("Account missing required login info (email or mobile and password required)")

    if email:
        payload = {
            "email": email,
            "password": password,
            "device_id": "deepseek_to_api",
            "os": "android",
        }
    else:
        payload = {
            "mobile": mobile,
            "area_code": None,
            "password": password,
            "device_id": "deepseek_to_api",
            "os": "android",
        }

    resp = requests.post(
        DEEPSEEK_LOGIN_URL,
        headers=BASE_HEADERS,
        json=payload,
        impersonate="safari15_3",
    )
    data = resp.json()
    resp.close()

    if data.get("data") is None or data["data"].get("biz_data") is None:
        raise ValueError(f"Login failed: invalid response format")

    new_token = data["data"]["biz_data"]["user"].get("token")
    if not new_token:
        raise ValueError("Login failed: missing token")

    _account["token"] = new_token
    save_config(CONFIG)
    _token = new_token
    return new_token


def get_token() -> str:
    """Get current token, login if needed."""
    global _token
    if not _token:
        init_single_account()
    return _token


def get_auth_headers() -> dict:
    """Get headers with authorization."""
    return {**BASE_HEADERS, "authorization": f"Bearer {get_token()}"}

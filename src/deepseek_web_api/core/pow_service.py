"""PoW calculation service."""

import base64
import json
import logging

from curl_cffi import requests

from .config import DEEPSEEK_CREATE_POW_URL
from .auth import get_auth_headers
from .pow import compute_pow_answer

logger = logging.getLogger(__name__)


def get_pow_response() -> str | None:
    """Get PoW response for completion endpoint."""
    headers = get_auth_headers()
    resp = requests.post(
        DEEPSEEK_CREATE_POW_URL,
        headers=headers,
        json={"target_path": "/api/v0/chat/completion"},
        impersonate="safari15_3",
    )
    data = resp.json()
    resp.close()

    if data.get("code") != 0:
        return None

    challenge = data["data"]["biz_data"]["challenge"]
    answer = compute_pow_answer(
        challenge["algorithm"],
        challenge["challenge"],
        challenge["salt"],
        challenge["difficulty"],
        challenge["expire_at"],
        challenge["signature"],
        challenge["target_path"],
    )

    pow_dict = {
        "algorithm": challenge["algorithm"],
        "challenge": challenge["challenge"],
        "salt": challenge["salt"],
        "answer": answer,
        "signature": challenge["signature"],
        "target_path": challenge["target_path"],
    }
    return base64.b64encode(json.dumps(pow_dict, separators=(",", ":")).encode()).decode()

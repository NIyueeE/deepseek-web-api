"""Local API authentication for protecting this proxy service."""

import secrets

from fastapi import HTTPException, Request, status

from .config import get_auth_required, get_enabled_auth_tokens


def _extract_request_token(request: Request) -> str:
    """Extract API token from Authorization or X-API-Key headers."""
    x_api_key = request.headers.get("x-api-key", "").strip()
    if x_api_key:
        return x_api_key

    authorization = request.headers.get("authorization", "").strip()
    if authorization.lower().startswith("bearer "):
        return authorization[7:].strip()

    return ""


def requires_local_api_auth(path: str) -> bool:
    """Return True when the request path should be protected."""
    return path.startswith("/v0") or path.startswith("/v1")


def verify_local_api_auth(request: Request) -> None:
    """Validate local API auth against enabled auth tokens."""
    expected_tokens = get_enabled_auth_tokens()
    auth_required = get_auth_required()

    if not auth_required and not expected_tokens:
        return

    provided_token = _extract_request_token(request)
    if provided_token:
        for expected_token in expected_tokens:
            if secrets.compare_digest(provided_token, expected_token):
                return

    raise HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail="Invalid or missing local API key",
        headers={"WWW-Authenticate": "Bearer"},
    )

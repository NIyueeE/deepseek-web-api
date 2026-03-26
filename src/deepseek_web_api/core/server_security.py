"""Server-side security warnings for local deployment."""

import logging

from .config import (
    get_auth_mode_summary,
    get_auth_required,
    get_cors_origins,
    get_server_host,
    has_effective_auth_tokens,
)
from .logger import logger


_LOOPBACK_HOSTS = {"127.0.0.1", "localhost", "::1"}


def is_loopback_host(host: str) -> bool:
    normalized = host.strip().lower().strip("[]")
    return normalized in _LOOPBACK_HOSTS


def collect_startup_security_warnings() -> list[str]:
    host = get_server_host()
    auth_required = get_auth_required()
    has_tokens = has_effective_auth_tokens()
    cors_origins = get_cors_origins()

    warnings = []
    if auth_required and not has_tokens:
        warnings.append("Auth is required but no enabled auth tokens are configured; /v0 and /v1 will reject every request with 401.")

    if not auth_required and not has_tokens:
        warnings.append("Local API auth is disabled; /v0 and /v1 are usable by any caller that can reach this service.")

    if "*" in cors_origins:
        warnings.append("CORS allows all origins; narrow [server].cors_origins before exposing browser clients.")

    if not is_loopback_host(host):
        warnings.append(f"Server host is {host}, not loopback; this service may be reachable from other machines.")

    if not is_loopback_host(host) and not auth_required and not has_tokens:
        warnings.append("Non-loopback binding without local API auth is unsafe for shared networks or public hosts.")

    return warnings


def log_startup_security_warnings() -> None:
    summary_message = f"[security] {get_auth_mode_summary()}"
    if logger.isEnabledFor(logging.INFO):
        logger.info(summary_message)
    else:
        logger.warning(summary_message)

    for warning in collect_startup_security_warnings():
        logger.warning(f"[security] {warning}")

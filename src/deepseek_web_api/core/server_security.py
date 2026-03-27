"""Server-side security warnings for local deployment."""


from .config import (
    get_auth_tokens,
    get_cors_origins,
    get_server_host,
)
from .logger import logger


_LOOPBACK_HOSTS = {"127.0.0.1", "localhost", "::1"}


def is_loopback_host(host: str) -> bool:
    normalized = host.strip().lower().strip("[]")
    return normalized in _LOOPBACK_HOSTS


def validate_startup_config() -> None:
    """Validate critical security configuration at startup.

    Raises SystemExit if non-loopback host is configured without auth tokens.
    """
    host = get_server_host()
    tokens = get_auth_tokens()

    if not is_loopback_host(host) and not tokens:
        logger.error(
            f"[security] CRITICAL: Non-loopback host '{host}' configured without auth tokens. "
            "This is unsafe. Either:\n"
            "  1. Set host to 127.0.0.1 for local-only access, or\n"
            "  2. Configure auth.tokens in config.toml"
        )
        raise SystemExit(1)


def collect_startup_security_warnings() -> list[str]:
    host = get_server_host()
    tokens = get_auth_tokens()
    cors_origins = get_cors_origins()

    warnings = []

    if not tokens and is_loopback_host(host):
        warnings.append("Local API auth is disabled (no tokens configured); safe for loopback only.")

    if "*" in cors_origins:
        warnings.append("CORS allows all origins; narrow [server].cors_origins before exposing browser clients.")

    if not is_loopback_host(host):
        warnings.append(f"Server host is {host}, not loopback; this service may be reachable from other machines.")

    return warnings


def log_startup_security_warnings() -> None:
    host = get_server_host()
    tokens = get_auth_tokens()

    if tokens:
        logger.info(f"[security] Auth enabled with {len(tokens)} token(s)")
    elif is_loopback_host(host):
        logger.warning("[security] Auth disabled (no tokens configured). Safe for loopback only.")

    for warning in collect_startup_security_warnings():
        logger.warning(f"[security] {warning}")

"""DeepSeek Web API routes."""

from fastapi import FastAPI, HTTPException, Request, Response
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse, StreamingResponse

from ..core.config import (
    get_cors_allow_credentials,
    get_cors_allow_headers,
    get_cors_allow_methods,
    get_cors_origin_regex,
    get_cors_origins,
)
from ..core.local_api_auth import requires_local_api_auth, verify_local_api_auth
from ..core.logger import logger
from ..core.server_security import log_startup_security_warnings

from .v0_service import (
    stream_chat_completion,
    stream_edit_message,
    delete_session as delete_session_service,
    create_session,
    upload_file,
    fetch_files,
    get_history_messages,
)

# API path constants - kept for reference, actual paths defined in v0_service.py

def get_cors_middleware_options() -> dict:
    options = {
        "allow_origins": get_cors_origins(),
        "allow_credentials": get_cors_allow_credentials(),
        "allow_methods": get_cors_allow_methods(),
        "allow_headers": get_cors_allow_headers(),
    }
    origin_regex = get_cors_origin_regex()
    if origin_regex:
        options["allow_origin_regex"] = origin_regex
    return options


app = FastAPI()

app.add_middleware(CORSMiddleware, **get_cors_middleware_options())


@app.on_event("startup")
async def startup_security_warnings():
    """Log security warnings once when the server worker starts."""
    log_startup_security_warnings()


@app.middleware("http")
async def local_api_auth_middleware(request: Request, call_next):
    """Protect /v0 and /v1 endpoints with optional local API key auth."""
    if requires_local_api_auth(request.url.path):
        try:
            verify_local_api_auth(request)
        except HTTPException as exc:
            return JSONResponse(
                status_code=exc.status_code,
                content={"detail": exc.detail},
                headers=getattr(exc, "headers", None),
            )
    return await call_next(request)


@app.api_route("/v0/chat/completion", methods=["POST"])
async def completion(request: Request):
    """Send chat completion with streaming SSE response."""
    body = await request.json()
    prompt = body.pop("prompt")
    search_enabled = body.pop("search_enabled", True)
    thinking_enabled = body.pop("thinking_enabled", True)
    client_chat_session_id = body.pop("chat_session_id", None)
    ref_file_ids = body.get("ref_file_ids", [])

    logger.info(f"[completion] prompt={prompt[:50]}..., session={client_chat_session_id}")

    # Pre-create session if needed to return session_id in header
    response_headers = {}
    if client_chat_session_id is None:
        chat_session_id, _ = await create_session()
        client_chat_session_id = chat_session_id
        response_headers["X-Chat-Session-Id"] = chat_session_id

    async def stream_and_set_header():
        async for chunk in stream_chat_completion(
            prompt=prompt,
            chat_session_id=client_chat_session_id,
            search_enabled=search_enabled,
            thinking_enabled=thinking_enabled,
            ref_file_ids=ref_file_ids,
        ):
            yield chunk

    logger.info(f"[completion] streaming started, session={client_chat_session_id}")
    return StreamingResponse(
        stream_and_set_header(),
        media_type="text/event-stream",
        headers=response_headers or None,
    )


@app.api_route("/v0/chat/message", methods=["POST"])
async def message(request: Request):
    """Edit message with fixed message_id=1 for stateless multi-turn conversation."""
    body = await request.json()
    prompt = body.pop("prompt")
    search_enabled = body.pop("search_enabled", True)
    thinking_enabled = body.pop("thinking_enabled", True)
    client_chat_session_id = body.pop("chat_session_id", None)

    logger.info(f"[message] prompt={prompt[:50]}..., session={client_chat_session_id}")

    # Pre-create session if needed to return session_id in header
    response_headers = {}
    if client_chat_session_id is None:
        chat_session_id, _ = await create_session()
        client_chat_session_id = chat_session_id
        response_headers["X-Chat-Session-Id"] = chat_session_id

    async def stream_and_set_header():
        async for chunk in stream_edit_message(
            prompt=prompt,
            chat_session_id=client_chat_session_id,
            search_enabled=search_enabled,
            thinking_enabled=thinking_enabled,
        ):
            yield chunk

    logger.info(f"[message] streaming started, session={client_chat_session_id}")
    return StreamingResponse(
        stream_and_set_header(),
        media_type="text/event-stream",
        headers=response_headers or None,
    )


@app.api_route("/v0/chat/delete", methods=["POST"])
async def delete_session(request: Request):
    """Delete session."""
    body = await request.json()
    chat_session_id = body.get("chat_session_id")
    logger.info(f"[delete] session={chat_session_id}")

    return await delete_session_service(chat_session_id)


@app.api_route("/v0/chat/create_session", methods=["POST"])
async def create_session_route(request: Request):
    """Create new session."""
    body = await request.json()
    agent = body.get("agent", "chat")
    logger.info(f"[create_session] agent={agent}")

    _, resp = await create_session({"agent": agent})
    return resp


@app.api_route("/v0/chat/upload_file", methods=["POST"])
async def upload_file_route(request: Request):
    """Upload file."""
    form = await request.form()
    file = form.get("file")
    if not file:
        return Response(content="No file provided", status_code=400)

    logger.info(f"[upload_file] filename={file.filename}")
    file_content = await file.read()
    return await upload_file(file_content, file.filename, file.content_type)


@app.api_route("/v0/chat/fetch_files", methods=["GET"])
async def fetch_files_route(request: Request):
    """Fetch file status."""
    file_ids = request.query_params.get("file_ids")
    logger.info(f"[fetch_files] file_ids={file_ids}")

    return await fetch_files(file_ids)


@app.api_route("/v0/chat/history_messages", methods=["GET"])
async def history_messages_route(request: Request):
    """Get chat history."""
    chat_session_id = request.query_params.get("chat_session_id")
    offset = int(request.query_params.get("offset", "0"))
    limit = int(request.query_params.get("limit", "20"))
    logger.info(f"[history_messages] session={chat_session_id}, offset={offset}, limit={limit}")

    return await get_history_messages(chat_session_id, offset, limit)


@app.get("/")
async def index():
    return {"status": "ok", "service": "deepseek-web-api"}

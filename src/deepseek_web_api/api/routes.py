"""DeepSeek Web API routes."""

import re
import logging

from fastapi import FastAPI, Request, Response
from fastapi.middleware.cors import CORSMiddleware
import httpx

from ..core.auth import get_auth_headers
from ..core.pow_service import get_pow_response
from ..core.session_store import SessionStore
from ..core.config import DEEPSEEK_HOST

logger = logging.getLogger("deepseek_web_api")

app = FastAPI()

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

DEEPSEEK_BASE_URL = f"https://{DEEPSEEK_HOST}"
session_store = SessionStore.get_instance()


def parse_sse_response_message_id(content: bytes) -> int | None:
    """Parse SSE stream to extract response_message_id from first data line."""
    try:
        text = content.decode("utf-8")
        for line in text.split("\n"):
            line = line.strip()
            if line.startswith("data:") and '"id":' in line:
                # Match "id": followed by 10+ digits (message_id is 18 digits)
                match = re.search(r'"id"\s*:\s*(\d{10,})', line)
                if match:
                    return int(match.group(1))
    except Exception:
        pass
    return None


async def proxy_to_deepseek(method, path, headers=None, json_data=None, params=None, content=None, files=None):
    """Proxy request to DeepSeek backend."""
    url = f"{DEEPSEEK_BASE_URL}/{path}"
    auth_headers = get_auth_headers()
    if headers:
        headers = {**headers, **auth_headers}
    else:
        headers = auth_headers
    headers["Host"] = DEEPSEEK_HOST

    async with httpx.AsyncClient(timeout=120.0) as client:
        resp = await client.request(
            method=method,
            url=url,
            headers=headers,
            json=json_data,
            params=params,
            content=content,
            files=files,
        )
        return Response(
            content=resp.content,
            status_code=resp.status_code,
            headers=dict(resp.headers),
        )


# ============ Routes ============

async def create_session_on_deepseek() -> str | None:
    """Create session on DeepSeek backend and return the chat_session_id."""
    import json
    resp = await proxy_to_deepseek(
        "POST",
        "api/v0/chat_session/create",
        json_data={"agent": "chat"},
    )
    if resp.body:
        try:
            data = json.loads(resp.body)
            csid = data.get("data", {}).get("biz_data", {}).get("id")
            if csid:
                session_store.create_session(csid)
                return csid
        except Exception:
            pass
    return None


@app.api_route("/v0/chat/completion", methods=["POST"])
async def completion(request: Request):
    """Send chat completion."""
    body = await request.json()
    prompt = body.pop("prompt")
    search_enabled = body.pop("search_enabled", True)
    thinking_enabled = body.pop("thinking_enabled", True)
    client_chat_session_id = body.pop("chat_session_id", None)

    new_session_created = False

    # Determine chat_session_id
    if client_chat_session_id:
        chat_session_id = client_chat_session_id
        parent_message_id = session_store.get_parent_message_id(chat_session_id)
        if chat_session_id not in session_store.get_all_sessions():
            session_store.create_session(chat_session_id)
            parent_message_id = None
    else:
        # No chat_session_id: create session on DeepSeek first
        chat_session_id = await create_session_on_deepseek()
        parent_message_id = None
        new_session_created = True

    # Get PoW
    pow_response = get_pow_response()

    # Build payload for DeepSeek
    payload = {
        "chat_session_id": chat_session_id,
        "parent_message_id": parent_message_id,
        "preempt": False,
        "prompt": prompt,
        "ref_file_ids": body.get("ref_file_ids", []),
        "search_enabled": search_enabled,
        "thinking_enabled": thinking_enabled,
    }

    headers = {"x-ds-pow-response": pow_response} if pow_response else {}

    # Proxy to DeepSeek
    deepseek_resp = await proxy_to_deepseek(
        "POST",
        "api/v0/chat/completion",
        headers=headers,
        json_data=payload,
    )

    # Extract response_message_id from SSE and update session_store
    if deepseek_resp.body:
        msg_id = parse_sse_response_message_id(deepseek_resp.body)
        if msg_id and chat_session_id:
            session_store.update_parent_message_id(chat_session_id, msg_id)

    # If we created a new session, add header with chat_session_id
    if new_session_created and chat_session_id:
        response_headers = dict(deepseek_resp.headers)
        response_headers["X-Chat-Session-Id"] = chat_session_id
        return Response(
            content=deepseek_resp.body,
            status_code=deepseek_resp.status_code,
            headers=response_headers,
        )

    return deepseek_resp


@app.api_route("/v0/chat/delete", methods=["POST"])
async def delete_session(request: Request):
    """Delete session."""
    body = await request.json()
    chat_session_id = body.get("chat_session_id")

    session_store.delete_session(chat_session_id)

    return await proxy_to_deepseek(
        "POST",
        "api/v0/chat_session/delete",
        json_data={"chat_session_id": chat_session_id},
    )


@app.api_route("/v0/chat/create_session", methods=["POST"])
async def create_session(request: Request):
    """Create new session."""
    import json
    body = await request.json()
    resp = await proxy_to_deepseek(
        "POST",
        "api/v0/chat_session/create",
        json_data=body,
    )

    # Parse returned chat_session_id, store it, and return it explicitly
    chat_session_id = None
    if resp.body:
        try:
            data = json.loads(resp.body)
            biz_data = data.get("data", {}).get("biz_data", {})
            chat_session_id = biz_data.get("id")
            if chat_session_id:
                session_store.create_session(chat_session_id)
                # Return with explicit chat_session_id at top level
                data["chat_session_id"] = chat_session_id
                return Response(
                    content=json.dumps(data),
                    status_code=resp.status_code,
                    headers={"Content-Type": "application/json"},
                )
        except Exception:
            pass

    return resp


@app.api_route("/v0/chat/upload_file", methods=["POST"])
async def upload_file(request: Request):
    """Upload file."""
    form = await request.form()
    file = form.get("file")
    if not file:
        return Response(content="No file provided", status_code=400)

    file_content = await file.read()
    files = {"file": (file.filename, file_content, file.content_type)}

    return await proxy_to_deepseek(
        "POST",
        "api/v0/file/upload_file",
        files=files,
    )


@app.api_route("/v0/chat/fetch_files", methods=["GET"])
async def fetch_files(request: Request):
    """Fetch file status."""
    file_ids = request.query_params.get("file_ids")
    return await proxy_to_deepseek(
        "GET",
        "api/v0/file/fetch_files",
        params={"file_ids": file_ids},
    )


@app.api_route("/v0/chat/history_messages", methods=["GET"])
async def history_messages(request: Request):
    """Get chat history."""
    chat_session_id = request.query_params.get("chat_session_id")
    offset = request.query_params.get("offset", "0")
    limit = request.query_params.get("limit", "20")
    return await proxy_to_deepseek(
        "GET",
        "api/v0/chat/history_messages",
        params={"chat_session_id": chat_session_id, "offset": offset, "limit": limit},
    )


@app.get("/")
async def index():
    return {"status": "ok", "service": "deepseek-web-api"}

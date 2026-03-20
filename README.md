# DeepSeek Web API

[English](./README.md) | [中文](./README.中文.md)

Transparent proxy for DeepSeek Chat API with automatic authentication and PoW calculation.

## Features

- **Automatic Authentication**: Server manages account credentials, no client-side auth required
- **PoW (Proof of Work)**: Automatic PoW challenge solving
- **Session Management**: Multi-turn conversation support via `chat_session_id`
- **SSE Streaming**: Pass-through SSE responses from DeepSeek

## Quick Start

```bash
# Configure account
cp config.toml.example config.toml
# Edit config.toml with your DeepSeek credentials

# Run server
uv run python main.py
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v0/chat/completion` | POST | Send chat message, streaming SSE |
| `/v0/chat/create_session` | POST | Create new session |
| `/v0/chat/delete` | POST | Delete session |
| `/v0/chat/history_messages` | GET | Get chat history |
| `/v0/chat/upload_file` | POST | Upload file |
| `/v0/chat/fetch_files` | GET | Query file status |

### Endpoint Details

#### POST /v0/chat/completion
**External**: Accepts `prompt`, optional `chat_session_id`, returns SSE stream.

**Internal**:
- No `chat_session_id` → Creates session via `POST /api/v0/chat_session/create`, stores locally, returns `X-Chat-Session-Id` header
- Has `chat_session_id` → Looks up `parent_message_id` from local store, appends to request
- Adds `Authorization`, `x-ds-pow-response` headers, proxies to DeepSeek
- Parses SSE to extract `response_message_id`, updates local session store

#### POST /v0/chat/create_session
**External**: Accepts `{"agent": "chat"}`, returns DeepSeek session data.

**Internal**:
- Proxies to `POST /api/v0/chat_session/create`
- Extracts `chat_session_id` from response, stores in local session map
- Returns DeepSeek response with explicit `chat_session_id` field at top level

#### POST /v0/chat/delete
**External**: Accepts `{"chat_session_id": "..."}`, returns DeepSeek response.

**Internal**:
- Removes session from local session store
- Proxies to `POST /api/v0/chat_session/delete`

#### GET /v0/chat/history_messages
**External**: Query params `chat_session_id`, `offset`, `limit`, returns message history.

**Internal**:
- Adds `Authorization` header, proxies to `GET /api/v0/chat/history_messages`

#### POST /v0/chat/upload_file
**External**: Multipart form with `file` field, returns DeepSeek response.

**Internal**:
- Reads file from form, proxies to `POST /api/v0/file/upload_file`

#### GET /v0/chat/fetch_files
**External**: Query param `file_ids` (comma-separated), returns file status.

**Internal**:
- Adds `Authorization` header, proxies to `GET /api/v0/file/fetch_files`

See [API.md](./API.md) for detailed documentation.

## TODO

- [x] Simple wrapper for deepseek_web_chat API
- [ ] Implement claude_message protocol proxy
- [ ] Implement openai_chat_completions protocol proxy

## Architecture

```
Client --> DeepSeek Web API --> DeepSeek Backend
              |
              +-- Authentication (auto-managed)
              +-- PoW solving
              +-- Session state management
```

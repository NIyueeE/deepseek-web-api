# DeepSeek Web API

[English](./README.md) | [中文](./README.中文.md)

Inspired by [deepseek2api](https://github.com/iidamie/deepseek2api). Transparent proxy for DeepSeek Chat API with automatic authentication and PoW calculation.

## Features

- **Automatic Authentication**: Server manages account credentials, no client-side auth required
- **PoW (Proof of Work)**: Automatic PoW challenge solving for chat and file upload
- **Session Management**: Multi-turn conversation support via `chat_session_id`
- **SSE Streaming**: Pass-through SSE responses from DeepSeek
- **File Upload**: Upload files and reference them in conversations via `ref_file_ids`
- **OpenAI Compatible API**: `/v1/chat/completions` endpoint with full tool calling support
- **Streaming Tool Calls**: Extract and convert tool call markers `[TOOL🛠️]...[/TOOL🛠️]` to OpenAI `delta.tool_calls` format

## Quick Start

```bash
# Configure account
cp config.toml.example config.toml
# Edit config.toml with your DeepSeek credentials

# Run server
uv run python main.py
```

**Note**: Only single-user mode is supported to prevent excessive load on DeepSeek's servers. Multi-user requests will not be implemented.

## Configuration

`config.toml` is required before running:

```toml
[account]
email = "your_email@example.com"      # DeepSeek account email
password = "your_password"           # DeepSeek account password
token = "your_deepseek_token"       # DeepSeek auth token (optional if email/password provided)
```

**Security**: The `/v1/chat/completions` endpoint has no API token verification. **Always run the service on `127.0.0.1`** (default in `main.py`) to prevent unauthorized access.

## Models

Available models via `/v1/models`:

| Model | Description |
|-------|-------------|
| `deepseek-web-chat` | Standard chat model, thinking disabled |
| `deepseek-web-reasoner` | Reasoning model with chain-of-thought thinking |

**Note**: Internal search functionality is disabled by default (no web search).

## Usage Example

[AstrBot](https://github.com/AstrBotDevs/AstrBot) integration with streaming reasoning and tool calls:

![AstrBot with deepseek-web-reasoner](./assets/reasoner-show.png)

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/chat/completions` | POST | OpenAI-compatible chat completions with tool support |
| `/v0/chat/completion` | POST | Send chat message, streaming SSE |
| `/v0/chat/create_session` | POST | Create new session |
| `/v0/chat/delete` | POST | Delete session |
| `/v0/chat/history_messages` | GET | Get chat history |
| `/v0/chat/upload_file` | POST | Upload file |
| `/v0/chat/fetch_files` | GET | Query file status |

### Endpoint Details

#### POST /v1/chat/completions
OpenAI-compatible chat completions endpoint with full tool calling support.

**Features**:
- Accepts OpenAI-style `messages` array with roles: `system`, `user`, `assistant`, `tool`
- Supports `tool_calls` in assistant messages for multi-turn tool conversations
- Tool results passed as `role: "tool"` with `tool_call_id` and `content`
- Streaming: Extracts `[TOOL🛠️]...[/TOOL🛠️]` markers and converts to `delta.tool_calls` chunks
- Non-streaming: Extracts tool calls from full response text
- Model-based behavior: `deepseek-web-reasoner` enables thinking/reasoning content

**Request body**:
```json
{
  "model": "deepseek-web-reasoner",
  "messages": [
    {"role": "user", "content": "What's the weather?"}
  ],
  "stream": false,
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get weather for a city",
        "parameters": {
          "type": "object",
          "properties": {"city": {"type": "string"}},
          "required": ["city"]
        }
      }
    }
  ]
}
```

**Response** (with tool calls):
```json
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "...",
      "tool_calls": [{
        "id": "call_xxx",
        "type": "function",
        "function": {"name": "get_weather", "arguments": "{\"city\": \"Beijing\"}"}
      }]
    },
    "finish_reason": "tool_calls"
  }]
}
```

### Endpoint Details

#### POST /v0/chat/completion
**External**: Accepts `prompt`, optional `chat_session_id`, optional `ref_file_ids`, returns SSE stream.

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
**External**: Multipart form with `file` field, returns file ID with `PENDING` status.

**Internal**:
- Reads file from form, calculates PoW for `/api/v0/file/upload_file` endpoint
- Adds `Authorization`, `x-ds-pow-response`, `x-file-size` headers
- Proxies to `POST /api/v0/file/upload_file`

#### GET /v0/chat/fetch_files
**External**: Query param `file_ids` (comma-separated), returns file status.

**Internal**:
- Adds `Authorization` header, proxies to `GET /api/v0/file/fetch_files`
- File status: `PENDING` = parsing, `SUCCESS` = done, `FAILED` = error

See [API.md](./API.md) for detailed documentation.

## Implementation Notes

### OpenAI Adapter (`/v1/chat/completions`)
The OpenAI-compatible adapter works by injecting prompts into the internal `/v0/chat/completion` endpoint:
- Converts OpenAI `messages` array to a prompt format with role markers
- Injects tool schemas into system instructions with `[TOOL🛠️]...[/TOOL🛠️]` response format
- Parses streaming SSE responses to extract tool call markers in real-time
- Supports multi-turn conversations by passing tool results back as `[TOOL_RESULT]` markers

## TODO

- [x] Simple wrapper for deepseek_web_chat API
- [x] Implement openai_chat_completions protocol adapter
- [x] Streaming tool call extraction for openai adapter
- [ ] Implement claude_message protocol adapter via [litellm](https://github.com/BerriAI/litellm) (convert OpenAI protocol to Claude protocol)

## Architecture

```
Client --> DeepSeek Web API --> DeepSeek Backend
              |
              +-- Authentication (auto-managed)
              +-- PoW solving
              +-- Session state management
```

## Disclaimer

DeepSeek's official API is very affordable. Please support the official service.

This project was created to experience the latest grayscale-tested models on the official web version.

**Commercial use is strictly prohibited** to avoid putting pressure on DeepSeek's servers. Use at your own risk.

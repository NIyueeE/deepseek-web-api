<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/ds-free-api/main/assets/logo.svg" width="81" height="66">
</p>

<h1 align="center">DS-Free-API</h1>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/NIyueeE/ds-free-api.svg"></a>
  <img src="https://img.shields.io/github/v/release/NIyueeE/ds-free-api.svg">
  <img src="https://img.shields.io/badge/rust-1.95.0+-93450a.svg">
  <img src="https://github.com/NIyueeE/ds-free-api/actions/workflows/ci.yml/badge.svg">
</p>
<p align="center">
  <img src="https://img.shields.io/github/stars/NIyueeE/ds-free-api.svg">
  <img src="https://img.shields.io/github/forks/NIyueeE/ds-free-api.svg">
  <img src="https://img.shields.io/github/last-commit/NIyueeE/ds-free-api.svg">
</p>

[中文](README.md)

A Rust API proxy that translates DeepSeek's free web chat into standard OpenAI and Anthropic-compatible API protocols (supports chat completions, the Responses API and messages, including streaming and tool calling).

## Highlights

- **Zero-cost API proxy**: Uses DeepSeek's free web interface — no official API key needed, get OpenAI/Anthropic-compatible endpoints for free
- **Triple protocol support**: OpenAI Chat Completions, the OpenAI Responses API (`/v1/responses` plus `GET /v1/responses/{id}` retrieval) and the Anthropic Messages API — drop-in compatible with mainstream clients
- **Tool call ready**: Full OpenAI function calling implementation with a 3-tier self-healing pipeline (text repair → JSON repair → model fallback), covering 10+ malformed formats
- **File upload ready**: Inline data URL files in OpenAI `file`/`image_url` content parts and Anthropic `image`/`document` content blocks are automatically uploaded to DeepSeek sessions; HTTP URLs trigger search mode so the model can access link content directly
- **Oversized prompt fallback**: When the prompt exceeds model limits, automatically falls back to chunked completion with file upload
- **Web admin panel**: Built-in dashboard for account pool status, API key management, request logs, i18n (Simplified Chinese / English / Bahasa Indonesia), theme switcher, responsive layout (desktop / tablet / mobile) and PWA install — hot-reloadable config, ready out of the box
- **Built with Rust**: Single binary + single TOML config, cross-platform native performance (web panel compiled in at build time)
- **Multi-account pool**: Idle-aware round-robin selection (DashMap lock-free reads), horizontal scaling for concurrency

## Quick Start

### Binary Usage

1. Download and extract the archive for your platform from [releases](https://github.com/NIyueeE/ds-free-api/releases)
2. Copy `config.example.toml` to `config.toml` and fill in accounts (optional — you can also configure via the admin panel after startup)
3. Run `./ds-free-api`
4. Visit `http://127.0.0.1:22217/admin` to set an admin password, then manage API keys and accounts from the panel

```bash
./ds-free-api
./ds-free-api -c /path/to/config.toml
RUST_LOG=debug ./ds-free-api
```

> **Concurrency**: The free API has session-level rate limits. This project has built-in rate-limit detection + exponential backoff retry for stability.
> Recommended parallelism = accounts / 2. Supports starting without `config.toml` and adding accounts via the admin panel.

### Docker Usage

```bash
docker compose -f docker/docker-compose.yaml up -d
```

Refer to the [sample compose file](./docker/docker-compose.yaml) for reference.

The admin panel is at `http://localhost:22217/admin`. Set your admin password on first visit.
The `config/` and `data/` directories are bind-mounted into the container — config changes persist to the host automatically.

### Free Test Accounts

Please register your own. You can refer to the method in [issue #62](https://github.com/NIyueeE/ds-free-api/issues/62).

> **⚠️ Account risk-control status (2026-09)**: Upstream risk control has tightened significantly.
> Every test account previously published in this README or in the issues **is now dead**
> (`USER_IS_BANNED` / `user is muted` / `RISK_DEVICE_DETECTED`). This is not a bug in this project —
> it is an upstream policy change against shared accounts:
>
> | Upstream response | Meaning | What to do |
> |-------------------|---------|------------|
> | `biz_code=10 USER_IS_BANNED` | Account permanently banned | Unrecoverable — register a new account |
> | `biz_code=5 user is muted` | Temporary mute (response carries `mute_until`, usually weeks) | Re-login does not help; the account is left in `invalid` |
> | `biz_code=11 RISK_DEVICE_DETECTED` | Missing browser device fingerprint | Add `device_id` to that account's config (see below) |
>
> So do **not** rely on public test accounts. Use your own and set a `device_id` per account in
> `config.toml` or the admin panel, otherwise login is rejected by risk control.
>
> **✅ Update (2026-10): the missing risk-control token is now implemented.** Capturing a real
> browser session revealed that the official client attaches a short-lived token issued by
> `hif-leim.deepseek.com` to every `/chat/completion` request (`x-hif-leim`, TTL from the
> `x-hif-ttl` response header, 600s by default). This proxy sent no such header at all, so
> upstream could flag a request as non-official without any behavioural analysis — which matches
> the observed "muted after only 2 requests per account" reports. Since v0.5.0 `ds_core` fetches
> the token, refreshes it on TTL and attaches it to SSE requests (`hif_enabled = true` by default).
> See `docs/development.md` for details.

#### How to obtain `device_id`

`device_id` is the device-level fingerprint produced by the Shumei SDK. Upstream uses it to link
and profile accounts, so **each account should use its own `device_id`** (log in once per account in
a separate incognito window / browser profile) — do not reuse one value across accounts. The value
**cannot be faked** (a made-up base64 or plain string returns `RISK_DEVICE_DETECTED`); it must come
from a real browser. One capture stays valid long-term:

1. Open `https://chat.deepseek.com/sign_in` in Chrome and log in once (let the page fully load so the risk-control script initializes)
2. Open DevTools → Network and filter for `users/login`
3. Trigger a login, inspect that request's payload, and copy the `device_id` value
4. Add it to the account config:

   ```toml
   [[ds_core.accounts]]
   email = "you@example.com"
   mobile = ""
   area_code = ""
   password = "your-password"
   device_id = "value captured from the browser"
   ```

   The admin panel config page exposes the same field (leaving it empty keeps the existing server-side value).

> Shortcut: run `SMSdk.getDeviceId()` in the browser console (once `SMSdk` is ready) to read the value directly.


## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET    | `/`   | Redirect to admin panel |
| GET    | `/health` | Health check |
| POST   | `/v1/chat/completions` | Chat completions (streaming + tool calls) |
| POST   | `/v1/responses` | OpenAI Responses API (streaming + tools + `previous_response_id`) |
| GET    | `/v1/responses/{id}` | Retrieve a stored Response object (in-process bounded+TTL cache; 404 when missing) |
| GET    | `/v1/models` | List models |
| GET    | `/v1/models/{id}` | Model details |
| POST   | `/anthropic/v1/messages` | Anthropic Messages (streaming + tool calls) |
| GET    | `/anthropic/v1/models` | List models (Anthropic format) |
| GET    | `/anthropic/v1/models/{id}` | Model details (Anthropic format) |

The admin panel is at `/admin` — on first visit you'll be guided to set an admin password.

## Model Mapping

> **⚠️ Upstream has retired `expert` / `vision` (2026-09)**
>
> The `model_configs` array from `/api/v0/client/settings` states it plainly:
>
> | model_type | Name | enabled | switchable |
> |------------|------|---------|------------|
> | `default` | Fast mode | ✅ true | ✅ true |
> | `expert` | Expert mode | ❌ **false** | ❌ false |
> | `vision` | Vision mode | ❌ **false** | ❌ false |
>
> The web UI no longer offers a switcher for the latter two — **there is effectively
> only one model now**. This project therefore exposes `deepseek-default` only.
> The expert/vision endpoints still respond, but upstream marks them disabled, so
> enabling them leads to flaky requests. Set `model_types` explicitly if you must.

The `model_types` config in `config.toml` (default `["default"]`) maps to `deepseek-<type>`:

| OpenAI Model ID    | DeepSeek Mode  |
| ------------------ | -------------- |
| `deepseek-default` | Fast mode      |

**Bare model names work too**: `default` (case-insensitive) is equivalent to
`deepseek-default`, which helps clients that set `model` to a short name
(Claude Code, Codex — see issue #99).

Optional aliases via `model_aliases`, aligned by index with `model_types`. Empty strings are skipped:

```toml
# model_types   = ["default", "expert"]
# model_aliases = ["", "deepseek-v4-pro"]  → deepseek-v4-pro maps to expert (index 1)
model_aliases = []
```

The Anthropic compatibility layer uses the same model IDs via `/anthropic/v1/messages`.

### Capability Toggles

- **Deep thinking**: Enabled by default. To explicitly disable, include `"reasoning_effort": "none"` in the request body.
- **Web search**: Enabled by default (DeepSeek injects a stronger system prompt in search mode, improving tool call adherence). To explicitly disable, include `"web_search_options": {"search_context_size": "none"}` in the request body.
- **File upload**: Inline files (data URL) are auto-uploaded to DeepSeek sessions; HTTP URLs trigger search mode:

  **OpenAI endpoint:**
  ```json
  {"type": "file", "file": {"file_data": "data:text/plain;base64,...", "filename": "doc.txt"}}
  {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
  {"type": "image_url", "image_url": {"url": "https://example.com/img.jpg"}}
  ```

  **Anthropic endpoint:**
  ```json
  {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}}
  {"type": "document", "source": {"type": "base64", "media_type": "text/plain", "data": "..."}}
  {"type": "image", "source": {"type": "url", "url": "https://example.com/img.jpg"}}
  ```

### Tool Call Tag Hallucination

Built-in fuzzy matching handles variations (full-width `｜`<=>`|`, `▁`<=>`_`) for most formats. If the model outputs a different fallback tag, add it via the admin panel or in `config.toml` under `[ds_core]`:

```toml
tool_call.extra_starts = ["<|tool_call_begin|>", "<tool_calls>", "<tool_call>"]
tool_call.extra_ends = ["<|tool_call_end|>", "</tool_calls>", "</tool_call>"]
```

## Web Admin Panel

Visit `http://127.0.0.1:22217/admin` after starting the server:

- **Dashboard**: Request statistics, account pool status at a glance (switches to a card layout on mobile)
- **Accounts**: View/add/remove accounts (including the `device_id` field), manually re-login accounts in Error state
- **API Keys**: Create/delete API keys, masked display
- **Models**: Available models with details, plus built-in cURL / Python / Node.js snippets
- **Config**: Accounts, API keys, model types and tool-call tags
- **Settings**: Server / proxy / ds_core client parameters, and admin password change
- **Logs**: Recent request logs and runtime logs

The UI supports three languages (Simplified Chinese / English / Bahasa Indonesia), light/dark themes
(system / light / dark), a collapsible sidebar persisted to `localStorage`, and installs as a PWA.

<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/ds-free-api/main/assets/web_p1.png" alt="Dashboard Overview" width="700">
  <br>
  <em>Dashboard overview</em>
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/ds-free-api/main/assets/web_p2.png" alt="Config Page" width="700">
  <br>
  <em>Config editor page</em>
</p>

On first visit, you'll be guided to set an admin password (stored as bcrypt hash), then issued a JWT (24h validity). Password reset revokes old tokens.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `info` | Log level (`trace` / `debug` / `info` / `warn` / `error`) |
| `DS_DATA_DIR` | `.` (current dir) | Data directory for `logs/runtime.log` and `stats.json` |
| `DS_CONFIG_PATH` | `./config.toml` | Config file path (lower priority than `-c` flag) |

## Security

- **Admin panel**: JWT authentication + bcrypt password hash + login rate limiting (5 failures → 5-minute lockout)
- **API access**: API keys created via the admin panel (HashSet O(1) lookup)
- **CORS**: Configurable allowed origins, defaults to `http://localhost:22217`
- **Sensitive data**: Account IDs masked in response headers, request bodies excluded from logs, persisted files at 0600 permissions

## Development

### Design Philosophy

**A single `config.toml` reflects all runtime state.** Admin panel changes are instantly persisted to `config.toml` and hot-reloaded into the running service.

**No unnecessary runtime system dependencies.** The project prioritizes pure Rust or statically-linked dependencies (e.g., `rustls` → `wreq` with BoringSSL), ensuring a single binary with no external `.so`/`.dll` requirements — download and run.

### Architecture Diagram

```mermaid
flowchart TB
    %% ===== Theme =====
    classDef client fill:#eff6ff,stroke:#3b82f6,stroke-width:3px,color:#1d4ed8,rx:14,ry:14
    classDef gateway fill:#fffbeb,stroke:#f59e0b,stroke-width:3px,color:#92400e,rx:12,ry:12
    classDef openai_adapter fill:#f8fafc,stroke:#0a9e7b,stroke-width:2px,color:#334155,rx:10,ry:10
    classDef anthropic_compat fill:#f8fafc,stroke:#d07354,stroke-width:2px,color:#334155,rx:10,ry:10
    classDef ds_core fill:#f8fafc,stroke:#3964fe,stroke-width:2px,color:#1e40af,rx:10,ry:10
    classDef external fill:#fef2f2,stroke:#ef4444,stroke-width:3px,color:#991b1b,rx:6,ry:6

    %% ===== Nodes =====
    Client(["Client"]):::client

    subgraph GW ["HTTP Gateway Layer"]
        Handler(["Router / Auth / Serialization"]):::gateway
    end

    subgraph PL ["Protocol Layer"]
        direction TB

        subgraph AC ["Anthropic Compat"]
            A2O["Request<br/>Anthropic → OpenAI"]:::anthropic_compat
            O2A["Response<br/>OpenAI → Anthropic"]:::anthropic_compat
        end

        subgraph OA ["OpenAI Adapter"]
            ReqPipe["Request Pipeline<br/>Validation / Tool Extraction / Prompt Building"]:::openai_adapter
            RespPipe["Response Pipeline<br/>SSE Parsing / Format Conversion / Tool Repair"]:::openai_adapter
        end
    end

    subgraph CL ["Core Layer (ds_core)"]
        Pool["Account Pool Rotation"]:::ds_core
        PoW["PoW Solver"]:::ds_core
        Session["Session Orchestration<br/>Create/Destroy / History Upload"]:::ds_core
    end

    DeepSeek[("DeepSeek API")]:::external

    %% ===== Connections =====
    Client -->|"HTTP Request"| Handler

    Handler -->|"OpenAI Request"| ReqPipe
    Handler -->|"Anthropic Request"| A2O
    A2O -->|"OpenAI Request"| ReqPipe

    ReqPipe --> Pool
    Pool --> PoW
    PoW --> Session
    Session -->|"completion endpoint"| DeepSeek

    Session -.->|"DeepSeek SSE Stream"| RespPipe
    RespPipe -.->|"OpenAI Response"| Handler
    RespPipe -.->|"OpenAI Response"| O2A
    O2A -.->|"Anthropic Response"| Handler

    %% ===== Subgraph Styles =====
    style GW fill:#fffbeb,stroke:#f59e0b,stroke-width:2px,stroke-dasharray: 5 5
    style PL fill:#fafafa,stroke:#94a3b8,stroke-width:2px
    style AC fill:#fdf0ec,stroke:#d07354,stroke-width:2px
    style OA fill:#e6f7f3,stroke:#0a9e7b,stroke-width:2px
    style CL fill:#eef2ff,stroke:#3964fe,stroke-width:2px,stroke-dasharray: 5 5
```

### Data Pipeline

#### OpenAI (chat_completions) Pipeline:

```mermaid
flowchart TB
    %% ===== Theme =====
    classDef ds_core fill:#eef2ff,stroke:#3964fe,stroke-width:2.5px,color:#1e40af,rx:10,ry:10
    classDef openai_adapter fill:#e6f7f3,stroke:#0a9e7b,stroke-width:2.5px,color:#065f46,rx:10,ry:10
    classDef step fill:#fffbeb,stroke:#f59e0b,stroke-width:1.5px,color:#334155,rx:6,ry:6

    subgraph RQ ["Request Pipeline"]
        direction TB
        Q1["ChatCompletionsRequest"]:::openai_adapter
        Q2["Validation + Defaults"]:::step
        Q3["Extract tools/files + inject prompts"]:::step
        Q4["Build DeepSeek native tag prompt"]:::step
        Q5["Model mapping + capability toggles"]:::step
        Q6["Retry with exp. backoff<br/>1s→2s→4s→8s→16s"]:::step
        Q7["ChatRequest"]:::ds_core
    end

    subgraph RS1 ["Non-streaming Response"]
        direction TB
        OR1["ds_core SSE stream"]:::ds_core
        OR2["SSE frame parse<br/>ContentDelta / Usage"]:::step
        OR3["State machine merge<br/>contiguous text / accumulate usage"]:::step
        OR4["Chunk aggregation<br/>concat content / reasoning / tool_calls"]:::step
        OR5["ChatCompletionsResponse"]:::openai_adapter
    end

    subgraph RS2 ["Streaming Response"]
        direction TB
        OS1["ds_core SSE stream"]:::ds_core
        OS2["SSE frame parse + state machine"]:::step
        OS3["Chunk conversion<br/>DsFrame → ChatCompletionsResponseChunk"]:::step
        OS4["Tool call XML parse"]:::step
        OS5["Malformed tool call repair"]:::step
        OS6["Stop sequence detect + obfuscation"]:::step
        OS7["ChatCompletionsResponseChunk"]:::openai_adapter
    end

    Q1 --> Q2 --> Q3 --> Q4 --> Q5 --> Q6 --> Q7
    OR1 --> OR2 --> OR3 --> OR4 --> OR5
    OS1 --> OS2 --> OS3 --> OS4 --> OS5 --> OS6 --> OS7

    style RQ fill:#f8fafc,stroke:#0a9e7b,stroke-width:2px
    style RS1 fill:#f8fafc,stroke:#0a9e7b,stroke-width:2px
    style RS2 fill:#f8fafc,stroke:#0a9e7b,stroke-width:2px
```

#### Anthropic (messages) Pipeline:

```mermaid
flowchart TB
    %% ===== Theme =====
    classDef oai fill:#e6f7f3,stroke:#0a9e7b,stroke-width:2.5px,color:#065f46,rx:10,ry:10
    classDef anth fill:#fdf0ec,stroke:#d07354,stroke-width:2.5px,color:#7c3a2a,rx:10,ry:10
    classDef step fill:#fffbeb,stroke:#f59e0b,stroke-width:1.5px,color:#334155,rx:6,ry:6

    subgraph RQ ["Request Pipeline"]
        direction TB
        Q1["MessagesRequest"]:::anth
        Q2["Message expansion<br/>System prepend / text merge / image+document mapping"]:::step
        Q3["Tool mapping<br/>ToolUnion → OpenAI Tool"]:::step
        Q4["Capability toggle mapping<br/>thinking → reasoning_effort"]:::step
        Q5["ChatCompletionsRequest"]:::oai
    end

    subgraph RS3 ["Non-streaming Response"]
        direction TB
        AR1["ChatCompletionsResponse"]:::oai
        AR2["Content decomposition<br/>reasoning → Thinking<br/>content → Text<br/>tool_calls → ToolUse"]:::step
        AR3["ID mapping<br/>chatcmpl → msg<br/>call → toolu"]:::step
        AR4["MessagesResponse"]:::anth
    end

    subgraph RS4 ["Streaming Response"]
        direction TB
        AS1["ChatCompletionsResponseChunk stream"]:::oai
        AS2["Chunk state machine<br/>block type switch / index progression"]:::step
        AS3["Event mapping<br/>content → text_delta<br/>reasoning → thinking_delta<br/>tool_calls → input_json_delta"]:::step
        AS4["MessagesResponseChunk"]:::anth
    end

    Q1 --> Q2 --> Q3 --> Q4 --> Q5
    AR1 --> AR2 --> AR3 --> AR4
    AS1 --> AS2 --> AS3 --> AS4

    style RQ fill:#f8fafc,stroke:#d07354,stroke-width:2px
    style RS3 fill:#f8fafc,stroke:#d07354,stroke-width:2px
    style RS4 fill:#f8fafc,stroke:#d07354,stroke-width:2px
```

For detailed development guide (building, testing, Docker deployment, e2e testing, etc.), see [docs/development.md](./docs/development.md).

## License

[GNU General Public License v3.0](LICENSE)

[DeepSeek's official API](https://platform.deepseek.com/top_up) is very affordable — please support the official service.

This project was born from the desire to try the latest models in DeepSeek's web interface during grayscale testing.

**Commercial use is strictly prohibited** to avoid putting pressure on official servers. Use at your own risk.

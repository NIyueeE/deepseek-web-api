# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> This file serves dual duty as both `AGENTS.md` (the real file) and `CLAUDE.md` (symlink → `AGENTS.md`).
> Edit `AGENTS.md` directly; `CLAUDE.md` stays in sync automatically.

---

## Project Overview

Rust API proxy exposing free DeepSeek model endpoints. Translates standard OpenAI-compatible and Anthropic-compatible requests to DeepSeek's internal protocol with account pool rotation, PoW challenge handling, and streaming response support.

**Runtime:** Rust **1.95.0** (pinned in `rust-toolchain.toml`) with **edition 2024**.
**Workspace:** Cargo workspace with two crates — `ds-free-api` (binary + server + adapters) and `ds_core` (DeepSeek API client library).
**Build prerequisites:** `cmake`, `g++`, `libclang-dev` — required to compile `wreq` (BoringSSL).

**Key dependencies and why they exist:**
- `wasmtime` — executes DeepSeek's PoW WASM solver; the entire PoW system depends on this (pinned to 48.x, see `.cargo/audit.toml`)
- `tiktoken-rs` — client-side prompt token counting (DeepSeek returns 0 for `prompt_tokens`)
- `pin-project-lite` — underpins every streaming response wrapper (`ConverterStream`, `ToolCallStream`, `RepairStream`, `StopDetectStream`)
- `axum` / `wreq` — HTTP server and client respectively; `wreq` uses BoringSSL, and the TLS/HTTP2 fingerprint is selected by the `emulation` config (`okhttp4_12` = native Android app, default; `chrome136` = desktop Chrome) so that the fingerprint matches the UA / `client_platform` identity
- `tokio` with `signal` feature — async runtime with graceful shutdown on SIGTERM/SIGINT

---

## Architecture

### Module Structure

The project is a **Cargo workspace** with two crates:

**`ds_core/`** — standalone library crate for DeepSeek API interaction:
```
ds_core/src/
├── lib.rs           # Public API: re-exports DsCore, CoreError, etc.
├── accounts.rs      # Facade: Accounts struct, wraps pool/client/solver
├── accounts/        # Account sub-modules
│   ├── client.rs    # Raw HTTP client: API endpoints, Envelope parsing
│   ├── pool.rs      # Account pool: init, selection, AccountGuard
│   ├── hif.rs       # HIF risk-control token (x-hif-leim) fetch/cache
│   └── pow.rs       # PoW solver: wasmtime WASM loader, DeepSeekHashV1
├── chat.rs          # Facade: Chat struct, prompt-size dispatch
├── chat/            # Chat sub-modules
│   ├── request.rs   # Chat orchestration: 3 request paths (normal/file/chunk)
│   └── response.rs  # SSE parsing, StreamEvent protocol, GuardedStream
└── config.rs        # DsCoreConfig and AccountConfig types
```

**`ds-free-api`** (root crate) — binary + server + protocol adapters:
```
src/
├── main.rs              # Binary entry (~10 lines): init runtime_log, parse CLI, run server
├── lib.rs               # Public API surface: re-exports all public types
├── config.rs            # Config load/save, Arc<RwLock<Config>>
│
├── openai_adapter/      # OpenAI protocol adapter
│   ├── openai_adapter.rs # Facade: OpenAIAdapter, OpenAIAdapterError, StreamResponse
│   ├── types.rs         # Request/response structs (ChatCompletionsRequest, etc.)
│   ├── models.rs        # Model registry and listing endpoints
│   ├── request.rs       # Facade for request submodules (also contains #[cfg(test)] tests)
│   ├── request/         # Request pipeline: normalize → tools → files → prompt → resolver
│   │   ├── normalize.rs # Validation, default params
│   │   ├── tools.rs     # Tool definition → prompt injection
│   │   ├── files.rs     # Data URL → FilePayload, HTTP URL → search mode
│   │   ├── prompt.rs    # ChatML → DeepSeek native tags, tool injection
│   │   └── resolver.rs  # Model resolution, capability toggles
│   ├── response.rs      # Facade + StreamCfg struct
│   └── response/        # Response pipeline: converter → tool_parser → repair → stop_detect
│       ├── converter.rs # ConverterStream: StreamEvent → ChatCompletionsResponseChunk
│       └── tool_parser.rs   # ToolCallStream: XML tag detection, sliding-window repair
│
├── anthropic_compat.rs  # Facade: Anthropic protocol translator (on top of openai_adapter)
├── anthropic_compat/    # Anthropic compat submodules
│   ├── types.rs         # MessagesRequest/Response structs
│   ├── models.rs        # Anthropic-format model list generation
│   ├── request.rs       # Anthropic JSON → OpenAI request mapping
│   ├── response.rs      # Facade for response submodules
│   └── response/
│       ├── stream.rs    # OpenAI SSE → Anthropic SSE events
│       └── aggregate.rs # OpenAI JSON → Anthropic JSON
│
├── responses_adapter.rs # Facade: OpenAI Responses API translator (on top of openai_adapter)
├── responses_adapter/   # Responses API submodules
│   ├── types.rs         # ResponsesRequest, ResponseObject, usage, SSE serialization
│   ├── request.rs       # Responses JSON → ChatCompletionsRequest mapping
│   ├── response.rs      # Response object builder + SSE event state machine
│   └── store.rs         # Bounded + TTL cache backing previous_response_id
│
├── server.rs            # Facade: router, request-id + auth middleware, graceful shutdown
├── server/              # HTTP server submodules
│   ├── admin.rs         # Admin panel route handlers
│   ├── auth.rs          # JWT sign/verify, password setup/login, rate limiter
│   ├── idempotency.rs   # Idempotency-Key cache (bounded + 24h TTL) + SSE recorder
│   ├── error.rs         # ServerError: OpenAI + Anthropic error envelopes
│   ├── handlers.rs      # Business route handlers (OpenAI + Responses + Anthropic)
│   ├── runtime_log.rs   # File log redirection (stdout → runtime.log)
│   ├── stats.rs         # Request stats recording
│   ├── store.rs         # StoreManager: delegates admin/keys to Config::save()
│   └── stream.rs        # SseBody: wraps StreamResponse → axum::body::Body
```
**Additional resources:**
- `config.example.toml` — authoritative configuration reference with all fields documented
- `examples/adapter_cli.rs` + `examples/adapter_cli/` — debug CLI + JSON request samples
- `py-e2e-tests/` — Python e2e test suite (uv-managed, JSON-driven scenarios)
- `docker/Dockerfile` + `docker/entrypoint.sh` + `docker/docker-compose.yaml` — Docker deployment
  (ghcr.io image). The entrypoint seeds `$DS_CONFIG_PATH` from the bundled
  `docker/config.example.toml` (`host = "0.0.0.0"`) when it is missing/empty: a bind-mounted
  `/app/config` shadows the baked-in config, and without seeding the binary auto-creates the
  **code default** `host = "127.0.0.1"`, which makes the published port unreachable.
  Images ≤ v0.5.1 still have the old behaviour, so their docs require `cp docker/config.example.toml
  docker/config/config.toml` before the first `up -d`
- `docs/` — `code-style.md`（代码注释、命名、错误消息约定），`logging-spec.md`（日志级别、target、模块级过滤），`deepseek-prompt-injection.md`（DeepSeek 原生标签、工具调用注入策略），`development.md`（环境配置、首次启动、Release 构建），`responses-api.md`（Responses API 协议实现说明），`compat-audit.md`（对照上游规范的兼容性审计）
- `ds_core/raw-api-reference.md` — DeepSeek 后端 API 参考（端点、信封格式、SSE 增量协议、PoW、WAF 绕过）

### Binary / Library Split

`main.rs` is a ~10-line wrapper: init `runtime_log`, read `DS_DATA_DIR`, parse CLI args via `Config::load_with_args()` → `(Config, PathBuf)`, call `server::run(config, config_path)`. The crate can be built both as a library (`cargo build --lib`) and a binary (`cargo build --bin ds-free-api`). `lib.rs` defines the full public API surface.

### Workspace Crates

The workspace has two crates. Run `cargo` commands from the **workspace root**:

- **Root crate** (`ds-free-api`): binary, server, adapters — `cargo build`, `cargo test`
- **`ds_core/`**: standalone DeepSeek library — `cargo build -p ds_core`, `cargo test -p ds_core`

Cargo resolves from the workspace root, so commands run from inside `ds_core/` won't work. The `just` commands in this file also assume the workspace root.

### Facade Module Pattern

`accounts.rs`, `chat.rs`, `openai_adapter.rs`, `server.rs`, `request.rs`, `response.rs`, `anthropic_compat.rs`, `responses_adapter.rs` are **facades**:
- They declare submodules with `mod` (keeping implementation private)
- They re-export only the minimal public interface via `pub use`
- They sometimes contain `#[cfg(test)]` test modules

This means the file tree does not directly map to the public API. To understand what a module exposes externally, read its facade file, not the directory listing.

### StreamResponse Type

`StreamResponse` is the unifying bridge between adapter layers and the HTTP server:
- Every adapter's streaming method returns `StreamResponse` (a boxed `Stream<Item = Result<Bytes>> + Send`)
- `server/stream.rs::SseBody` wraps `StreamResponse` and converts it into an `axum::body::Body`
- This decouples the adapters from the HTTP framework — they produce bytes, the server handles SSE framing


### CI Build Pipeline

On tag push (`.github/workflows/release.yml`):

```
verify (tag == Cargo.toml == ds_core == web/package.json, CHANGELOG entry exists)
  └── build-frontend (bun install --frozen-lockfile + bun run build)
        └── test (downloads web-dist, then cargo test --workspace --all-targets)
              ├── build-linux-gnu  (cargo build --release --locked) │
              ├── build-linux-musl (cargo build --release --locked) │── release (tar.gz + zip + SHA256SUMS)
              ├── build-macos      (cargo build --release --locked) │
              └── build-windows    (cargo build --release --locked) │
              └── docker (ghcr.io image, provenance + SBOM)
```

Two gates run before any cross-compilation:

1. `verify` — seconds, no frontend needed: fails when the tag, `Cargo.toml`,
   `ds_core/Cargo.toml`, `web/package.json` or `CHANGELOG.md` disagree.
2. `test` — downloads `web-dist` **before** compiling, then runs the full suite. The
   frontend artifact is required here: see `build.rs` below.

`build.rs` turns the silent `rust_embed` failure mode into an explicit one: when
`web/dist/index.html` is missing, a **release** build fails with an explanatory panic,
while a debug build (e.g. `cargo check` before the frontend exists) only emits
`cargo:warning`. Without this, `cargo build --release` would succeed and ship a binary
with no admin panel at all.

`build-frontend` produces a `web-dist` artifact. Each platform build job downloads it
before compiling Rust, so `rust_embed` embeds the real frontend assets.

On PR/push (`.github/workflows/ci.yml`):

```
changes (paths-filter)
  ├── build-frontend (typecheck + lint + i18n key-set gate + build)
  ├── check          (check + clippy + fmt + audit + machete + outdated + lint-exemption gate)
  └── test           (cargo test --workspace --all-targets + doc tests)
  └── security       (cargo-deny: licences / banned crates / registry sources)
```

Documentation-only changes skip the Rust jobs via the `changes` gate.

### Frontend (`web/`)

Vite + React + shadcn/ui SPA under `web/`. Built by `bun run build` in `web/` (typecheck: `bun run typecheck`).
The binary embeds `web/dist/` via `rust_embed` at compile time.

```
web/
├── src/
│   ├── App.tsx            # Routes: /login + protected layout (dashboard/models/config/settings/logs) + SplashScreen
│   ├── lib/               # Shared libraries
│   │   ├── api.ts         # Typed API client for all admin endpoints (+ normalizeConfig, localizeAuthError)
│   │   ├── auth-context.ts # Auth context provider
│   │   ├── auth.tsx       # JWT auth context (localStorage token)
│   │   ├── use-auth.ts    # Auth hook for components
│   │   ├── theme.ts       # useTheme hook (system/light/dark, localStorage)
│   │   └── utils.ts       # Utility functions
│   ├── i18n/index.ts      # i18next init (zh / en / id)
│   ├── locales/           # zh/common.json, en/common.json, id/common.json (identical key sets)
│   ├── pages/             # ConfigPage, DashboardPage, Layout, LoginPage, LogsPage, ModelsPage, SettingsPage
│   └── components/        # LanguageSwitcher, ThemeSwitcher, UserDropdown, CodeSnippet, SplashScreen
│       └── ui/            # shadcn/ui primitives (badge, button, card, input, table, skeleton, etc.)
├── public/                # favicon.svg (symlink to assets/logo.svg), manifest.json, manifest.webmanifest, sw.js
├── e2e/capture-responsive.ts # Playwright responsive screenshot suite (port 22217)
├── index.html
├── package.json
└── vite.config.ts
```

The frontend includes i18n support (`web/src/i18n/`, `web/src/locales/{zh,en,id}/`) — all three
locales must keep **identical key sets**; adding a key to one file requires adding it to the other two.
Language switching lives in `LanguageSwitcher.tsx` / `UserDropdown.tsx`, theme switching
(system/light/dark) in `lib/theme.ts` + `ThemeSwitcher.tsx`.
Library files include `api.ts`, `auth.tsx`, `auth-context.ts`, `use-auth.ts`, `theme.ts`, and `utils.ts`.
Pages: `ConfigPage`, `DashboardPage`, `Layout`, `LoginPage`, `LogsPage`, `ModelsPage`, `SettingsPage`.

**Route-level code splitting**: every page in `App.tsx` is loaded through `React.lazy`
(`Suspense` fallback = skeleton rows), so the initial bundle only contains the shell + login page.
Keep new pages lazy — the frontend build warns above 500KB per chunk.

**Responsive + PWA**: the layout collapses to an icon rail on tablets and a bottom tab bar on
mobile; `SplashScreen.tsx` covers initial hydration, and `public/sw.js` (registered from
`index.html` under `/admin/`) uses network-first for navigations (so a new release is picked up
immediately) and stale-while-revalidate for hashed static assets, while passing `/admin/api/*`
straight to the network.

**Admin panel config editor**: `ConfigPage.tsx` fetches from `GET /admin/api/config`,
edits accounts / API keys / model types / tool-call tags and submits via `PUT /admin/api/config`
(full replace + hot-reload); `SettingsPage.tsx` handles server / proxy / ds_core fields, the
Responses API context cache, and admin password change. Passwords and `device_id` values sent
as `***`/empty are merged with existing values server-side.

Every index-aligned array (`max_input_tokens`, `max_output_tokens`, `input_character_limits`,
`model_aliases`) must stay the same length as `model_types` — `Config::validate()` rejects
mismatches. `normalizeConfig()` in `lib/api.ts` pads/truncates them defensively, and the
add/delete handlers in `ConfigPage.tsx` update all five arrays together. Default values in
the frontend must mirror `src/config.rs`'s `default_*` functions.

**Dev mode (HMR)**: Run `cd web && bun run dev` (Vite HMR) alongside `just serve`.
Backend reads from `web/dist/` filesystem when available.
---

## Principles

### 1. Single Responsibility
Every module has one job. Cross-module boundaries are strict:
- `config.rs`: Configuration load & save only, no client creation or business logic
- `client.rs`: Raw HTTP calls only, no token caching, retry, or SSE parsing
- `accounts.rs`: Account pool management only, no network requests
- `pow.rs`: WASM computation only, no account management or request sending
- `anthropic_compat.rs`: Protocol translation only, no direct `ds_core` access

### 2. Minimal Viable
- No premature abstractions: Extract traits/structs when needed, not before
- No redundant code: Remove unused imports, avoid over-documenting, no pre-written tests
- Delay dependency introduction: only add deps when actually needed

### 3. Control Complexity
- Explicit over implicit: Dependencies injected via parameters, no global state
- Composition over inheritance: Small modules composed via functions, no deep inheritance
- Clear boundaries: Modules interact via explicit interfaces, no internal logic leakage

---

## Key Architectural Patterns

### Account Pool Model

1 account = 1 session = 1 concurrency. Scale via more accounts in `config.toml`.

`AccountGuard` wraps `Arc<Account>`. It marks account as `busy` (via `AtomicBool`) on creation and releases on `Drop`. Held in `GuardedStream` to keep account busy during streaming.

### Account Initialization Flow

`AccountPool::init()` spins up accounts concurrently (capped at 13 via `tokio::sync::Semaphore`).
Each account runs `try_init_account()`:
1. `login` — obtain Bearer token (payload carries the account's `device_id`; **omitting it
   fails with `RISK_DEVICE_DETECTED`, biz_code 11** — verified empirically, see `docs/development.md`)
2. `create_session` — create a temporary chat session
3. `health_check` — test completion (with PoW) against `default` to verify a writable session
4. `delete_session` — always runs, including on health-check failure

There is **no retry inside `init()`** and **no `InitFailed` state** — a failure immediately
marks the account `Invalid`. The states are `Idle` / `Busy` / `Error` / `Invalid`.

Retries live in the background recovery task (`start_recovery_task`, every 60s): accounts in
`Error` are re-logged-in, and after `MAX_ERROR_COUNT` (3) consecutive failures they become `Invalid`.
**Terminal** login errors (`biz_code` 2 / 5 / 10 / 11 — wrong credentials, muted, banned,
device rejected) mark the account `Invalid` immediately via `is_terminal_login_error`:
retrying them only keeps hitting upstream, and the docs note that retrying a muted account
can extend the mute.

`update_title` exists in the raw client (`ds_core/src/accounts/client.rs`) but has **no call
site** — do not describe it as part of the init flow.

### Request Flow (per-chat)

`v0_chat()` → `get_account()` → `split_history()` → `create_session()` → `upload_files()` → `compute_pow()` → `completion()` → `parse_ready()` → `GuardedStream`

Each `v0_chat()` call creates a dedicated session, uploads multi-turn history as files, then streams the response. The response is a `StreamEvent` stream (not raw SSE bytes) — the new **精简响应协议** abstracts away DeepSeek's p/o/v patch protocol into typed events: `Meta`, `ThinkStart`, `ThinkDelta`, `ContentStart`, `ContentDelta`, `Done`. The session is destroyed when the stream ends via `GuardedStream::drop`, which also calls `stop_stream` on abnormal disconnects. Sessions are tracked in `active_sessions: Arc<Mutex<HashMap<String, ActiveSession>>>`.

The `Chat` module dispatches across 3 request paths based on prompt size:
- **Normal path** (`v0_chat_once`): prompt fits within model limit, sent directly
- **History-split path** (`v0_chat_oversized_file`): oversize non-expert model, splits history into uploaded files
- **Chunked path** (`v0_chat_oversized_chunk`): oversize `expert` model, uses chunked completion with file writes
  (only reachable if `expert` is explicitly enabled in `model_types`; upstream currently marks it `enabled: false`)

### Single-Struct Pipeline (OpenAI)

The adapter uses a **single struct** (`ChatCompletionsRequest`) through the entire request pipeline — no intermediate types:

```
ChatCompletionsRequest
  → normalize::apply |
  → tools::extract   |  reads ChatCompletionsRequest fields directly
  → files::extract   |
  → prompt::build    |
  → resolver::resolve|
  → try_chat (ds_core::ChatRequest)
  → if req.stream → ChatCompletionsResponseChunk | else → ChatCompletionsResponse
```(tiktoken 计数在 `OpenAIAdapter::chat_completions()` 内联完成，非独立 pipeline 模块)

### Response Pipeline (OpenAI) — Stream Chain

```
StreamEvent (ds_core) → ConverterStream (converter)
                      → ToolCallStream (tool_parser)
                      → (RepairStream — optional tool call repair)
                      → StopDetectStream (stop_detect + obfuscation)
                      → SSE bytes
```

SSE parsing and DeepSeek's p/o/v patch protocol are handled inside `ds_core/src/chat/response.rs`, which emits `StreamEvent` items. The adapter layer no longer touches raw bytes.

All stream wrappers use `pin_project_lite::pin_project!` macro and implement `Stream` with `poll_next`. Each wrapper is a pinned struct with an inner stream and state, using `Projection` to access fields in `poll_next`.

### Tool Calls via XML

Tool definitions are injected as **plain System message content, once** (see `docs/deepseek-prompt-injection.md`). Response tool-call XML is parsed back into structured JSON via `ToolCallStream`:

1. **Sliding window detector** accumulates content chunks and looks for `<tool_calls>` XML tags
2. **Fuzzy character normalization**: U+FF5C→|, U+2581→_
3. **JSON repair**: backslash escaping, unquoted keys
4. **Fallback tags**: configurable via `TagConfig.extra_starts`/`extra_ends` in `config.toml`
5. **`<invoke>` XML fallback** for alternative tag formats
6. `arguments` field normalized to always be a JSON string

Primary tag: `<tool_calls>` (plural). Configurable fallback tags via `TagConfig` in `config.toml`.

### History Splitting & File Upload

Multi-turn conversations split history at `split_history_prompt()`:
- The last user+assistant pair + final user message go **inline** in the prompt
- Earlier turns are wrapped in `[file content begin]`/`[file content end]` markers and uploaded as `EMPTY.txt`
- External files (data URLs) upload individually with a separate PoW computation targeting `/api/v0/file/upload_file`
- Upload polling: 3 attempts with 0.5/1/2s backoff, checking file existence via `fetch_files`

### Oversized Prompt Chunk Splitting

The expert chunked path slices the prompt with `split_prompt_chunks()` in `ds_core/src/chat/request.rs`:
- Boundaries are taken at `<｜Role｜>` tags; whole message blocks are greedily packed up to `chunk_size`
  (75% of `input_character_limits` for the model type), so a tag is never cut in half
  (a half tag such as `<｜Assista` / `nt｜>` makes upstream return an empty completion)
- A single message block larger than `chunk_size` falls back to a plain character split of that block
- If the prompt contains no tags at all, the whole prompt is character-split

Prompt history is also split at `split_history_prompt()` (see above), which parses the same
native `<｜Role｜>` tags via `parse_native_blocks()`.

### Capability Toggles

Request fields mapped in `request/resolver.rs`:
- **Reasoning**: defaults to `"high"` (on). Set `"none"` to disable.
- **Web search**: `web_search_options` explicitly enables it. When omitted, the
  `default_search_enabled` config flag decides (`true` by default, preserving the
  historical always-on behaviour; set `false` for strict OpenAI semantics). Prompt text
  containing an HTTP URL also forces search mode on.
- **File upload**: data URL content parts → auto upload to session; HTTP URLs → search mode.
- **Response format**: `response_format` → JSON/schema text injection in prompt.
- **Login `device_id`**: per-account field forwarded into the `/users/login` payload.
  It is **required in practice** — logging in without it is rejected with
  `RISK_DEVICE_DETECTED` (biz_code 11), verified empirically. **Each account should
  use its own** `device_id`: the fingerprint is device-scoped and upstream correlates
  accounts by it; sharing one across accounts raises mute risk. Startup warns when
  accounts share a fingerprint. See `docs/development.md`. The `X-Device-Id` header is
  **derived per account** from this fingerprint (`pool::account_x_device_id`), and the
  `x-hif-leim` token cache is keyed by that device id (`hif::HifRegistry`) — one device
  identity and one risk token per account, matching the real client.
- **`n` parameter**: OpenAI requires `n >= 1`; `n=0` and `n>1` are rejected with `400`
  (`request/normalize.rs`) because upstream only ever produces a single candidate.
- **Hourly request quota** (`hourly_request_quota`, default 60, 0 = unlimited): enforced
  per account in `AccountPool::get_account()` via a one-hour **sliding window**
  (`SlidingWindowRateLimiter` in `ds_core/src/accounts/pool.rs`; PR #114 replaced the
  earlier fixed window, which allowed bursts at window boundaries). Accounts over budget
  are skipped; if every account is over budget the request returns 429 instead of
  hammering upstream. This exists because upstream mutes accounts after a few hundred
  requests per hour, and muting is **delayed** — see `docs/development.md`.
- **Transport emulation** (`emulation`, default `okhttp4_12`): the TLS/HTTP2 fingerprint and
  the profile's default request headers. `okhttp4_12` keeps the "native Android app"
  identity self-consistent (no `sec-ch-ua*` / `sec-fetch-*`, `accept: */*`);
  `chrome136` suits a web identity (Chrome UA + `client_platform = web` + `client_os = web`).
  The 2026-09 note that "desktop Chrome UA is blocked by WAF 202" no longer holds — see
  `docs/development.md` (2026-10-07 identity probe).
- **HIF risk token** (`hif_enabled`, default true): `x-hif-leim`, fetched from
  `hif-leim.deepseek.com` per device identity (TTL from `x-hif-ttl`) and attached to SSE
  requests only. Disabling it is for A/B experiments only.

### Overloaded Retry

`OpenAIAdapter::try_chat()` retries up to **6 times** with **exponential backoff** (1s → 2s → 4s → 8s → 16s) on `CoreError::Overloaded`, triggered by DeepSeek's `rate_limit_reached` SSE hint or all accounts busy.

### Oversized Prompt Fallback

When the prompt exceeds `input_character_limits[type] * 75 / 100`, `v0_chat()` dispatches to a
fallback path in `ds_core/src/chat/request.rs`: `expert` uses chunked completion
(`v0_chat_oversized_chunk`), every other type uses history-split file upload
(`v0_chat_oversized_file`). There is no `oversized_prompt` config section — the threshold
comes from `input_character_limits`, which upstream currently reports as 2621440 for all
model types.

### Responses API Layer

Pure protocol translator on top of `openai_adapter` — no direct `ds_core` access:
- Request: `Responses JSON → request::into_chat_completions() → OpenAIAdapter::chat_completions()`
- Response: `ChatOutput::Stream → response::stream()` (SSE events) / `ChatOutput::Json → response::from_chat_completions()`
- `previous_response_id`: in-process, bounded (capacity) + TTL cache in `store.rs`; the
  cache is shared with the streaming response via a `FinishHook` invoked when the
  final `output` array is known
- Full protocol reference: `docs/responses-api.md`; compatibility audit:
  `docs/compat-audit.md`

### Anthropic Compatibility Layer

Pure protocol translator on top of `openai_adapter` — no direct `ds_core` access:
- Request: `Anthropic JSON → to_openai_request() → OpenAIAdapter::chat_completions() / try_chat()`
- Response: `OpenAI SSE/JSON → from_chat_completion_stream() / from_chat_completion_bytes() → Anthropic SSE/JSON`
- ID mapping: `chatcmpl-{hex}` → `msg_{hex}`, `call_{suffix}` → `toolu_{suffix}`
- `ToolUnion` in `types.rs` defaults to `Custom` type when absent (backward compat with Claude Code)

### Error Translation Chain

Errors propagate upward with translation at each module boundary:

1. **`ds_core/client.rs`**: `ClientError` (`Http` | `Status` | `Business` | `Json` | `InvalidHeader`)
   - Parses DeepSeek's wrapper envelope `{code, msg, data: {biz_code, biz_msg, biz_data}}` via `Envelope::into_result()`
2. **`ds_core/pool.rs`**: `PoolError` (`AllAccountsFailed` | `Client`(ClientError) | `Pow`(PowError) | `Validation` | `Exists`)
3. **`ds_core/lib.rs`**: `CoreError` (`Overloaded` | `ProofOfWorkFailed` | `ProviderError` | `Stream`)
4. **`openai_adapter.rs`**: `OpenAIAdapterError` (`BadRequest` | `Overloaded` | `ProviderError` | `Internal` | `ToolCallRepairNeeded`)
5. **`anthropic_compat.rs`**: `AnthropicCompatError` (`BadRequest` | `Overloaded` | `Internal`)
6. **`server/error.rs`**: `ServerError` (`Adapter`(OpenAIAdapterError) | `Anthropic`(AnthropicCompatError) | `Unauthorized` | `NotFound`(String))

All errors use `thiserror` derive macro.

### Request Tracing & Account Header

The outermost middleware (`request_id_middleware` in `server.rs`) mints a `req-{n}` ID per
request, stores it in request extensions and echoes it as the `x-request-id` response header —
including on middleware-generated errors (401), so clients can quote an ID that exists in the logs.
Handlers read it back via the `RequestId` extractor (`handlers.rs`) and thread it through the
adapter → `ds_core`. Key log points carry `req=` for cross-layer tracing:
```bash
RUST_LOG=debug 2>&1 | grep 'req=req-1'
```
The `x-ds-account` HTTP response header carries the account identifier upstream.

### Idempotent Retries (`Idempotency-Key`)

`server/idempotency.rs` implements the Stripe/OpenAI convention for the three POST endpoints
(`/v1/chat/completions`, `/v1/responses`, `/anthropic/v1/messages`):

- scope is `(API key, method+path, Idempotency-Key)`; a differing request body under the same key
  returns `400 idempotency_error` (fingerprint via `DefaultHasher`, process-local only);
- an entry still `InFlight` → `409`; a completed entry is replayed **byte-for-byte**
  (status + `Content-Type` + body) with `idempotent-replayed: true`;
- streaming responses are recorded chunk by chunk (`RecordingStream`); a client disconnect or a
  body over 1MB voids the entry so a retry really re-runs instead of replaying a truncated answer;
- the store is in-process, bounded (1024 entries / 1MB per entry / 64MB total) with a 24h TTL and
  is never persisted. **Without the header, behaviour is exactly as before** — that property is
  what keeps the feature safe to ship without new config.

### HTTP Routes

| Endpoint | Handler | Description |
|----------|---------|-------------|
| `GET /` | `server::root` | Redirect to /admin |
| `GET /health` | `server::health` | Health check (`{"status": "ok"}`) |
| `POST /v1/chat/completions` | `handlers::chat_completions` | OpenAI chat completion |
| `POST /v1/responses` | `handlers::responses` | OpenAI Responses API (SSE events or Response object) |
| `GET /v1/responses/{id}` | `handlers::responses_get` | Retrieve a stored Response object (in-process bounded+TTL cache; 404 when missing/evicted/expired) |
| `GET /v1/models` | `handlers::list_models` | List models |
| `GET /v1/models/{id}` | `handlers::get_model` | Get model |
| `POST /anthropic/v1/messages` | `handlers::anthropic_messages` | Anthropic messages |
| `GET /anthropic/v1/models` | `handlers::anthropic_list_models` | List models (Anthropic format) |
| `GET /anthropic/v1/models/{id}` | `handlers::anthropic_get_model` | Get model (Anthropic format) |

Bearer auth is **always enforced** on `/v1/*` and `/anthropic/*` via `[[api_keys]]`.
If `api_keys` is empty no token can validate, so every API request returns
`401 invalid_api_key` (`type: invalid_request_error`, matching OpenAI's own 401 body) —
create a key in the admin panel (or `[[api_keys]]`) first.

Two credential headers are accepted:
- `Authorization: Bearer <key>` — OpenAI SDK, and the Anthropic SDK's `authToken`
- `x-api-key: <key>` — the Anthropic SDK's default (`apiKey`), used by Claude Code

`/v1/*` and `/anthropic/*` use **separate middleware instances** so each returns the
error envelope its clients expect: OpenAI `{"error":{type,message,param,code}}` vs
Anthropic `{"type":"error","error":{type,message}}`.

### Model ID Mapping

`model_types` in `[ds_core]` config (default: `["default"]`) maps to OpenAI model ID
`deepseek-{type}`. Upstream's `/api/v0/client/settings` reports `default` as the only
`enabled`/`switchable` model type — `expert` and `vision` are both `enabled: false`, so the
default config exposes `deepseek-default` only. `model_registry()` also registers the bare
`{type}` name (`default`), which Claude Code / Codex rely on (issue #99).
Anthropic compat uses the same IDs.

---

## Conventions

### Code

```rust
// Import grouping: std → third-party → crate → local, separated by blank lines
use std::sync::Arc;

use serde::Deserialize;

use crate::config::Config;

use super::inner::Helper;
```

- **Visibility**: `pub(crate)` for types not part of the public API; facade modules keep submodules private with `mod`
- **Comments**: Chinese in source files (team preference)
- **Error messages**: Chinese for user-facing output; English for internal/debug
- **Naming**: `snake_case` for modules/functions, `PascalCase` for types/enum variants, `SCREAMING_SNAKE_CASE` for constants
- **Module files**: `foo.rs` declares sub-modules, `foo/` contains implementation

### Comments

Follow `docs/code-style.md`:
- `//!` — module docs: first line = responsibility, then key design decisions
- `///` — public API docs: verb-led, note side effects and panic conditions
- `//` — inline: explain "why", not "what"

### Logging

- `log` crate with **explicit targets**. Untargeted logs (e.g., bare `log::info!`) are prohibited.
- Targets used:
  - `ds_core::accounts`, `ds_core::client`
  - `adapter` (for `openai_adapter`)
  - `http::server`, `http::request`, `http::response` (for `server`)
  - `anthropic_compat`, `anthropic_compat::models`, `anthropic_compat::request`, `anthropic_compat::response`, `anthropic_compat::response::stream`, `anthropic_compat::response::aggregate`
  - `responses_adapter` (Responses API mapping, SSE state machine, `previous_response_id` cache)
- See `docs/logging-spec.md` for full target/level mapping

### Config

- Uncommented values in `config.toml` = required; commented = optional with default
- `src/config.rs` is the single source for config loading — no other module reads config files
- `Config::load_with_args()` returns `(Config, PathBuf)` — the path is propagated to `AppState.config_path` for reload
- In the server layer, `Config` is wrapped in `Arc<tokio::sync::RwLock<Config>>` — runtime-mutable, admin panel changes auto-persist via `Config::save()`
- `Config::save()` writes atomically (tmp + rename, 0600 permissions). `Config` includes `AdminConfig` (password hash, JWT secret) and `api_keys: Vec<ApiKeyEntry>` — no separate JSON files

### Testing

- All tests are inline (`#[cfg(test)]` within `src/` files). No separate `tests/` directory.
- `request.rs` has sync unit tests for parsing logic
- `response.rs` has `tokio::test` async tests for stream aggregation
- `server.rs` has `#[cfg(test)]` HTTP-layer tests driven by `tower::ServiceExt::oneshot`
  against stub routes (deterministic, no network). `tower` is a `[dev-dependency]`
  for exactly this purpose; bodies are read with `axum::body::to_bytes`.
- `println!`/`eprintln!` allowed inside `#[cfg(test)]` for debugging failures; prohibited in library code

## Anti-Patterns

- Do **NOT** create separate config entry points — `src/config.rs` is the single source
- Do **NOT** implement provider logic outside its `*_core/` module
- Do **NOT** commit `config.toml` (only `config.example.toml`)
- Do **NOT** use `println!`/`eprintln!` in library code — use `log` crate with target
- Do **NOT** use untargeted log macros — always specify `target: "..."`
- Do **NOT** access `ds_core` directly from `anthropic_compat` — always go through `OpenAIAdapter`
- Do **NOT** use `#[allow(...)]` in any file except `ds_core/src/accounts/client.rs` — dead API methods and deserialized fields for API symmetry are expected only in the raw HTTP client layer. New lint exemptions in other files must be resolved (refactor or consume the value) rather than suppressed.
- The workspace lint set in `Cargo.toml` is intentionally **strict** (rustc: `trivial_casts`,
  `elided_lifetimes_in_paths`, `let_underscore_drop`, `unused_lifetimes`, …; clippy:
  `needless_pass_by_value`, `assigning_clones`, `map_unwrap_or`, `format_push_string`,
  `cast_possible_truncation`, `cast_possible_wrap`, `cast_precision_loss`,
  `unchecked_time_subtraction`, `branches_sharing_code`, `significant_drop_tightening`,
  `items_after_statements`, `uninlined_format_args`, `missing_const_for_fn`, `unused_async`, …).
  These are chosen for signal: float→int saturation, timestamp wraparound, locks held across
  `await`, and needless allocations are all caught. When a new lint fires, fix the code; do not
  weaken the list without stating the reason in the commit message.
- Do **NOT** keep admin/auth config in separate JSON files (`admin.json`, `api_keys.json`) — they are merged into `Config` fields and persisted via `Config::save()` into `config.toml`
- Do **NOT** run `git checkout`, `git commit`, or `gh` commands without explicit user permission — always ask before destructive or persistent operations
---

## Troubleshooting

| Issue | Symptom | Likely Cause / Fix |
|-------|---------|--------------------|
| WASM load failure | `PowError::Execution` on startup | DeepSeek recompiled WASM. PowSolver now uses dynamic export probing (no hardcoded symbols). Update `wasm_url` in `config.toml` if WASM URL changed |
| WAF blocking (non-US) | AWS WAF Challenge response (status 202) | Configure a non-US proxy in `config.toml` `[proxy]` |
| WAF blocking (fingerprint) | HTTP 403 / connection reset / 202 challenge | `wreq` uses BoringSSL; the fingerprint is chosen by `emulation` (`okhttp4_12` default, `chrome136` for a web identity). Verify the identity is self-consistent (UA ↔ `client_platform` ↔ emulation) with `cargo run -p ds_core --example identity_probe` — it hits only unauthenticated endpoints, no account traffic |
| Account init failure | All accounts stuck in init | Bad credentials (login fails first) or rate-limited (too many sessions). Check `[accounts]` in config |
| Login fails with `RISK_DEVICE_DETECTED` (biz_code 11) | `客户端错误: Business error: code=11, msg=RISK_DEVICE_DETECTED` during account init | DeepSeek requires a browser-registered device fingerprint. Capture `device_id` from a real browser login (`POST /api/v0/users/login`) and set it per account in `config.toml` / the admin panel |
| Account init fails with `user is muted` (biz_code 5) | `账号配置错误: 账号异常(muted/limited)`, account left in `invalid` | The account is temporarily muted upstream (response carries `mute_until`, typically days). It cannot be recovered by re-login — stop using the account until `mute_until` passes; further retries do not speed it up. Muting is **delayed**, so "ran N requests without being muted" is not evidence that a change avoids risk control — see `docs/development.md` |
| Tool call parse failure | No `tool_calls` in response, raw XML visible | Model output a tag variant not in the parse list. Add fallback `extra_starts`/`extra_ends` in `config.toml` `[ds_core]` |
| Rate limited | Repeated `CoreError::Overloaded` | Add more accounts or reduce concurrency. 6x exponential backoff handles transient spikes |
| Session errors mid-stream | `invalid message id`, session not found | Usually handled by `GuardedStream::drop` cleanup. If persistent, check concurrent access to same account |
| API request always 401 | `{"error":{"message":"invalid api token"}}` on every `/v1/*` call | `api_keys` is empty or the token doesn't match; there is **no** auth-free mode. Add a key via the admin panel |
| Oversized prompt rejected | `413` or truncation errors | Prompt exceeds DeepSeek limit. The oversized fallback (history-split file upload / expert chunked completion) handles this automatically; tune `input_character_limits` in `[ds_core]` |
| `不支持的模型: default` / client reports model not found | Model selection fails in Claude Code / Codex (issue #99) | Bare model_type names are accepted since v0.2.11 (`default` == `deepseek-default`). On older builds use the `deepseek-` prefix or set `model_aliases` |
| Streaming stalls | No SSE events after initial connection | Check `RUST_LOG=adapter=trace,ds_core::accounts=debug,info` for where the pipeline halts |
| Working inside `ds_core/` | `cargo` not finding manifests | Run commands from workspace root, not from inside `ds_core/`. Use `cargo build -p ds_core` |

---

## Where to Look

| Task | Location | Notes |
|------|----------|-------|
| env vars | `src/config.rs` + `main.rs` | `DS_CONFIG_PATH` / `-c`, `DS_DATA_DIR` for data directory |
| Config loading | `src/config.rs` | Single unified entry, `-c` flag support |
| Config reference | `config.example.toml` | All fields documented with examples (authoritative); `docker/config.example.toml` must stay in sync (enforced by `scripts/check-config-drift.sh`) |
| DeepSeek chat flow | `ds_core/src/` | accounts → pow → completions → client |
| Chat orchestration + file upload | `ds_core/src/chat/request.rs` + `response.rs` | `v0_chat()`, history splitting, upload retry, `GuardedStream` |
| OpenAI request parsing | `src/openai_adapter/request/` | normalize → tools → files → prompt → resolver |
| File upload extraction | `src/openai_adapter/request/files.rs` | data URL → FilePayload, HTTP URL → search mode |
| Token counting (tiktoken) | `src/openai_adapter.rs` | `OpenAIAdapter::bpe` field, inlined in `chat_completions()` |
| OpenAI response conversion | `src/openai_adapter/response/` | converter → tool_parser → repair → stop_detect |
| Tool call parser & stop sequences | `src/openai_adapter/response/tool_parser.rs` | `TagConfig` with extra_starts/extra_ends; stop filtering embedded |
| Stream pipeline config | `src/openai_adapter/response.rs` | `StreamCfg` struct (consolidates 8 stream params) |
| Anthropic compat layer | `src/anthropic_compat/` | Built on openai_adapter, no direct ds_core access |
| Responses API layer | `src/responses_adapter/` | Request/response mapping, SSE event state machine, `previous_response_id` cache |
| Responses API protocol reference | `docs/responses-api.md` | Field tables, event sequence, store trade-offs, unimplemented list |
| Responses retrieval | `src/responses_adapter/store.rs` | `StoredTurn.response` keeps the full snapshot for `GET /v1/responses/{id}`; in-process bounded+TTL (restart loses it) |
| Compatibility audit | `docs/compat-audit.md` | Verified spec deltas for Chat Completions / Anthropic / Responses |
| Anthropic streaming response | `src/anthropic_compat/response/stream.rs` | OpenAI SSE → Anthropic SSE event stream |
| Anthropic aggregate response | `src/anthropic_compat/response/aggregate.rs` | OpenAI JSON → Anthropic JSON |
| OpenAI protocol types | `src/openai_adapter/types.rs` | Request/response structs, `#![allow(dead_code)]` |
| Model listing | `src/openai_adapter/models.rs` | Model registry and listing |
| HTTP server/routes | `src/server/` | handlers → stream → error; `request_id_middleware` mints `req-{n}` + `x-request-id` |
| Idempotency cache | `src/server/idempotency.rs` | `Idempotency-Key` scope/conflict/replay rules + `RecordingStream` for SSE replay |
| PoW WASM solver | `ds_core/src/accounts/pow.rs` | wasmtime loading, dynamic export probing, DeepSeekHashV1 |
| DeepSeek HTTP client | `ds_core/src/accounts/client.rs` | `Envelope::into_result()`, WAF detection, all API methods |
| HIF risk-control token | `ds_core/src/accounts/hif.rs` | `x-hif-leim`: polled from `hif-leim.deepseek.com/query` (no auth), TTL from `x-hif-ttl`, attached to SSE requests only. Missing it marks the request as a non-official client — see `docs/development.md` (2026-10-07) |
| Unified debug CLI | `examples/adapter_cli.rs` | Modes: chat/raw/compare/concurrent/status/models |
| Risk-token probe | `examples/hif_probe.rs` | Checks the `hif-leim` endpoint only — **no account traffic** |
| Account status check | `examples/account_check.rs` | Login-only `is_muted` / `mute_until` check (1 request per account) |
| Identity/WAF probe | `ds_core/examples/identity_probe.rs` | Compares client-identity variants against the WAF using unauthenticated endpoints + throwaway credentials — **no account traffic** |
| Example request JSON | `examples/adapter_cli/` | Pre-built ChatCompletionsRequest samples |
| Scripted regression test | `just adapter-cli -- source examples/adapter_cli-script.txt` | Runs all JSON samples in sequence |
| Docker deployment | `docker/Dockerfile` + `docker/entrypoint.sh` + `docker/docker-compose.yaml` | Pre-built ghcr.io image, bind mounts for config/data. First-run config seeding lives in the entrypoint (bind mount shadows the baked config); ≤ v0.5.1 images need a manual `cp` |
| e2e scenario test framework | `py-e2e-tests/` | JSON-driven scenarios with checks |
| CI pipeline | `.github/workflows/ci.yml` | `changes` gate + `build-frontend` + `check` + `test` + `security` |
| Dependency audit policy | `.cargo/audit.toml` | Documented upstream warnings that cannot be fixed here (wreq 5.x yanked, transitive lru unsound) |
| Dependency licence/ban policy | `deny.toml` | cargo-deny: licence allow-list, banned crates, registry sources. `graph.targets` is restricted to the 5 shipped targets so Windows-only transitive crates don't skew licence checks; `[[licenses.clarify]]` pins `wreq-util` (its `GPL-3.0` SPDX id is deprecated) |
| Licence declarations | `Cargo.toml` / `ds_core/Cargo.toml` | Both crates must declare `license` (else cargo-deny reports `unlicensed`); the path dependency in `Cargo.toml` must carry an explicit `version` (else `wildcards = "deny"` fails) |
| Frontend embed guard | `build.rs` | Fails `--release` builds (warns on debug) when `web/dist/index.html` is missing, because `rust_embed` embeds nothing silently |
| Outdated wrapper | `scripts/check-outdated.sh` | `cargo outdated` fails to resolve because wreq 5.x is yanked; script skips only that known case |
| Lint exemption gate | `scripts/check-lint-exemptions.sh` | Enforces the "no `#[allow]` outside client.rs" rule in CI |
| i18n key-set gate | `web/scripts/check-locales.mjs` | Fails CI when the three locale files diverge |
| Frontend config parity gate | `web/scripts/check-config-parity.mjs` | Every field in `config.example.toml` must appear somewhere under `web/src` (or be listed in `web/scripts/config-parity-allowlist.txt` with a reason). Guards the "backend adds a field, admin panel writes it back empty" class of bug |
| Config example drift gate | `scripts/check-config-drift.sh` | Root and `docker/` config examples must stay identical except `host` |
| Config example (authoritative) | `config.example.toml` + `docker/config.example.toml` | Both must document every config field; the docker copy differs only in `host` |
| Responses e2e | `py-e2e-tests/test_responses.py` | `just e2e-responses` — streaming, tools, `previous_response_id`, error envelopes |
| Release workflow | `.github/workflows/release.yml` | Tag `v*` → 8 targets, 4 platforms, CHANGELOG release |
| Code style | `docs/code-style.md` | 注释、命名、错误消息约定 |
| Logging spec | `docs/logging-spec.md` | 日志级别、目标、模块级过滤 |
| Prompt injection strategy | `docs/deepseek-prompt-injection.md` | DeepSeek 原生标签、工具调用注入策略 |
| DeepSeek API reference | `ds_core/raw-api-reference.md` | 端点、信封格式、SSE 增量协议、PoW 算法 |
| Development guide | `docs/development.md` | 环境配置、首次启动、Release 构建 |
| Admin panel routes | `src/server/admin.rs` | Setup/login/config/status/stats/models/logs handlers |
| JWT auth + password | `src/server/auth.rs` | `setup_admin()`/`login_admin()`, JWT sign/verify, login rate limiter |
| Store manager | `src/server/store.rs` | API key validation, stats persistence, delegates admin/keys to `Config::save()` |
| Request stats | `src/server/stats.rs` | `RequestStats`, `StatsHandle`, background flush to `stats.json` |
| Runtime log | `src/server/runtime_log.rs` | stdout redirect to `runtime.log` with rotation |
| Web admin panel | `web/src/pages/` | Dashboard/Config/Settings/Logs/Models pages, `lib/api.ts` API client |
| i18n locales | `web/src/locales/{zh,en,id}/common.json` | Keep the three key sets identical; see `web/src/i18n/index.ts` |
| PWA assets | `web/public/` | `manifest.webmanifest`, `sw.js` (registered under `/admin/`) |
| Responsive screenshots | `web/e2e/capture-responsive.ts` | Playwright; needs a running server on port 22217 |

---

## Release Checklist（发布前必须逐项通过）

> **教训（v0.5.1）**：第一次打 `v0.5.1` tag 时没有走完这份清单 —— 本地 release 构建、
> 真实账号 E2E、最终提交冻结都没做，tag 打在了一个仍有并发缺陷的提交上，只能删 tag 重打。
> 这正是要避免的 **fix-on-fix**：**清单跑完之前不要 `git tag`；tag 推送之后不要再改代码。**
>
> 每一项都要有可复核的证据（命令 + 输出/记录），不接受「应该没问题」。

1. **冻结代码**：计划内的改动全部已提交，`git status --short` 为空。清单执行期间不改代码；
   若验证发现缺陷 → 修完**从第 1 步重来**，绝不在打 tag 之后继续改。
2. **本地门禁全绿**（在待发布的那个 commit 上）：
   - `cargo fmt --all --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace --all-targets`
   - `scripts/check-lint-exemptions.sh`、`scripts/check-config-drift.sh`
   - `cargo audit`、`cargo machete`、`cargo deny --all-features check licenses bans sources`
     （本机未安装时以 CI 的 `check` / `security` 作业为准，并在发布记录里注明）
   - `cd web && bun run typecheck && bun run lint && bun run check:locales && bun run check:config-parity && bun run build`
3. **版本一致性**：`Cargo.toml` == `ds_core/Cargo.toml` == `web/package.json` == `CHANGELOG.md`
   的条目版本 == 即将推送的 tag。
4. **本地 release 构建 + 冒烟**：`cargo build --release --locked`，用**这个二进制**启动并验证
   `/health`、`/admin/`（内嵌前端）、`/v1/models`、401 错误信封，以及本次新增/修改的端点与行为。
5. **真实账号 E2E（凡是改动请求链路的版本必做）**：
   - **一次只用一个账号**，把请求数压到最少（能合并的验证合并进同一次请求）；
   - 至少覆盖 OpenAI 流式、Anthropic 流式、Responses API，以及本次改动直接影响的行为
     （例：验证 `Idempotency-Key` 回放时必须证明**没有**产生第二次上游请求）；
   - 记录账号、时间、结果；涉及风控相关改动时，写明观察窗口与尚未验证的边界。
6. **清理与复核**：没有遗留的临时文件 / 本地 tag / draft release；
   `git diff <上一个已发布 tag>..HEAD` 逐段过一遍，确认没有意外改动（新增依赖、`#[allow]`、调试代码）。
7. **一次到位地打 tag**：`git push origin main` → `git tag -a vX.Y.Z` → `git push origin vX.Y.Z`。
8. **CI 全绿**：Release 工作流的 `verify` / `test` / 8 个平台产物 / Docker 镜像全部成功。
9. **发布产物冒烟**：下载 Release 的 Linux 产物，实际运行并复验 `/health`、`/v1/models`、
   `GET /v1/responses/{id}` 等关键路径。
10. **最后才发布**：draft release 只有在第 9 步通过后才 `gh release edit --draft=false`。
    **已发布的 tag 不得移动** —— 需要修正就发下一个 patch 版本。

---

## Commands

```bash
# Setup (config auto-created on first run; copy example only if you want defaults)

# Enable pre-commit hook (check + clippy + fmt + audit + machete + cargo test)
git config core.hooksPath .githooks

# One-pass check (check + clippy + fmt + audit + unused deps)
just check

# Run the HTTP server with basic logging
just serve
RUST_LOG=info just serve
# Trace through the entire SSE pipeline
RUST_LOG=adapter=trace,ds_core::accounts=debug,info just serve
# Module-level logging filters
RUST_LOG=ds_core::accounts=debug,ds_core::client=warn,info just serve
RUST_LOG=adapter=debug,anthropic_compat=debug just serve

# Run unified protocol debug CLI (modes: chat, raw, compare, concurrent N, status, models, model <id>)
just adapter-cli
RUST_LOG=debug just adapter-cli
# Script mode — runs all JSON samples in sequence (full regression)
just adapter-cli -- source examples/adapter_cli-script.txt
# Interactive mode with a specific config
cargo run --example adapter_cli -- -c /path/to/config.toml

# Run specific test modules
just test-adapter-request
just test-adapter-response
cargo test responses_adapter              # Responses API request/response/store tests
cargo test server::                       # HTTP auth middleware + error envelope tests
just test-adapter-request converter_emits_role_and_content -- --exact

# Run a single Rust test (use -- --exact for precise name matching)
cargo test converter_emits_role_and_content -- --exact

# Risk-control diagnostics (no account traffic / login-only)
cargo run --example hif_probe                     # 只探测 x-hif-leim 端点连通性
cargo run --example account_check -- -c config.toml   # 只登录一次，读 is_muted/mute_until

# Run all Rust tests
cargo test

# Run only library tests (skips example compilation, faster iteration)
cargo test --lib

# e2e tests (requires `uv`, server on port 22217)
just e2e-basic     # Basic: 基础功能测试（OpenAI + Anthropic 双端点）
just e2e-repair    # Repair: 工具调用损坏修复专项测试
just e2e-stress    # Stress: 全部场景 × 3 次迭代压测
just e2e-responses # Responses: /v1/responses（流式 + 工具 + previous_response_id）
# See docs/development.md for full e2e CLI parameters (filter, parallel, model, report, etc.)

# Start server with e2e config
just e2e-serve

# Individual checks
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo audit        # requires: cargo install cargo-audit
cargo machete      # requires: cargo install cargo-machete
cargo deny --all-features check licenses bans sources   # requires: cargo install cargo-deny
scripts/check-outdated.sh  # wraps `cargo outdated --exit-code 1 --root-deps-only`
                   # (skips only the known wreq-5.x-yank resolution failure)
scripts/check-lint-exemptions.sh
just check-web     # frontend typecheck + lint + i18n key-set gate + build

# Build
cargo build
cargo build --release

# Release (tag push triggers CI: 8 targets, 4 platforms, aarch64 on ARM runners)
git tag v0.x.x
git push origin v0.x.x
# CI extracts changelog from CHANGELOG.md, creates GitHub release
```
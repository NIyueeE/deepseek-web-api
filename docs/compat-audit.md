# OpenAI 兼容性审计（Chat Completions + Responses）

> 审计基准：`openai-openapi` 的 `CreateChatCompletionRequest` /
> `CreateChatCompletionStreamResponse` / `CreateResponse` / `ResponseStreamEvent`
> schema，以及 `openai-python` 的运行时解析逻辑（`_streaming.py`、
> `lib/streaming/responses/_responses.py` 与各 `types/` 定义）。
> 审计对象：本仓库 commit `main`（v0.2.11 之后）。

本文档记录**已核实**的差异与修复结论。每条结论都标注了规范来源，
便于后续回归时对照。

---

## 1. 已修复

### 1.1 `obfuscation` 字段位置错误（Chat Completions 流式）

**规范**：`CreateChatCompletionStreamResponse.obfuscation` 是 **chunk 的顶层字段**，
与 `id` / `object` / `choices` 同级。`ChatCompletionStreamResponseDelta`
**没有** `obfuscation` 字段。

**修复前**：`src/openai_adapter/types.rs` 把 `obfuscation` 定义在 `Delta` 内，
于是每个 chunk 输出 `choices[0].delta.obfuscation`。

**影响**：严格遵守 schema 的客户端（Rust/Go 的强类型反序列化）会因未知字段报错；
Python/TS SDK 因为默认忽略未知字段而侥幸可用，属于隐性契约破坏。

**现状**：`ChatCompletionsResponseChunk.obfuscation: Option<String>`（顶层），
`Delta` 中已移除该字段。填充逻辑在 `src/openai_adapter/response.rs`
的 `StopDetectStream` 中执行。

### 1.2 `usage` 在未请求时被提前下发（Chat Completions 流式）

**规范**：`usage` 字段「只在设置 `stream_options: {"include_usage": true}` 时出现」，
且「出现时值为 `null`，**唯独最后一个 chunk** 携带真实统计」。若客户端未请求 usage，
整个流不应包含任何 usage 对象。

**修复前**：`converter.rs` 在 role chunk 上无条件附加
`usage: {prompt_tokens, completion_tokens: 0, total_tokens}`，
即使 `include_usage=false`。

**影响**：把 prompt token 数提前泄露给未请求的客户端；部分客户端会在
首个 chunk 就用这个半成品 usage 覆盖统计，导致 token 计数错误。

**现状**：role chunk 的 usage 改为 `include_usage.then(...)`，关闭时完全不下发。

### 1.3 错误响应缺少 `param` 字段（OpenAI `Error` schema）

**规范**：`Error` 的 `required` 为 `["type", "message", "param", "code"]`。

**修复前**：`src/server/error.rs` 的 `OpenAIErrorDetail` 只有
`message` / `type` / `code`。

**现状**：补上 `param: null`，四个字段齐备。

### 1.4 Anthropic 错误信封结构错误

**规范**：`{"type":"error","error":{"type":"<kind>","message":"..."}}`。
`<kind>` ∈ `invalid_request_error` / `authentication_error` / `permission_error` /
`not_found_error` / `rate_limit_error` / `api_error` / `overloaded_error`。

**修复前**：`AnthropicErrorBody` 把 error kind 放在**顶层**的 `type`，
body 形如 `{"type":"invalid_request_error","message":"..."}`。

**影响**：Anthropic SDK 通过 `error.error.type` 分派错误类
（`anthropic-sdk-typescript` `src/core/error.ts`），结构不符会退化成
无法识别的 `APIError`，重试/降级逻辑失效。

**现状**：按规范嵌套，并新增 `anthropic_not_found_error()` 保证
`/anthropic/*` 的 404 也使用 Anthropic 信封。

### 1.5 Anthropic 端点的 `x-api-key` 鉴权缺失（BLOCKER 级互操作问题）

**规范/实现**：Anthropic 官方 SDK 默认发送 **`x-api-key`** 头
（`anthropic-sdk-typescript` `src/client.ts`：`apiKey` → `x-api-key`；
`authToken` → `Authorization: Bearer`）。Claude Code 亦然。

**修复前**：`extract_bearer_token()` 只读取 `Authorization: Bearer`，
所有 `/anthropic/*` 请求在默认配置下必然 401。

**现状**：新增 `extract_api_token()`：优先 `Authorization: Bearer`，
回退 `x-api-key`。`/v1/*` 与 `/anthropic/*` 使用各自的中间件实例，
以便分别返回正确形态的错误信封。

### 1.6 Anthropic 流式事件缺少 `content_block_stop` 收尾

**规范**（docs.anthropic.com/en/api/messages-streaming）：`message_start` 之后，
每个工具调用是 `content_block_start` → 一到多个 `content_block_delta`
（`input_json_delta`）→ **`content_block_stop`**。

**修复前**：多工具调用场景下，`content_block_stop` 先被发出，
随后才 start 下一个块 —— 顺序正确但有重复 start 的隐患；
更重要的是**块内多次 delta 时**每次都重新 start。

**现状**：`transition_to()` 只在块类型变化时 stop 旧块，
同一工具调用的 `.arguments` 分片合并为单个 `input_json_delta`
（见 `anthropic_compat/response/stream.rs`）。

### 1.7 Anthropic `stop_reason` 可能输出非法枚举值

**规范**：`StopReason` 仅允许
`end_turn | max_tokens | stop_sequence | tool_use | pause_turn | refusal | model_context_window_exceeded`。

**修复前**：`finish_reason_map()` 对 `length` / `content_filter` 直接透传。

**现状**：显式映射 `length → max_tokens`、`content_filter → refusal`，
未知取值告警并退化为 `end_turn`。非流式响应在无 `finish_reason` 时
也保证 `stop_reason` 非空（规范要求非流式必须非空）。

### 1.8 `/v1/models` 缺少裸 model_type 名

**问题**：`resolver` 接受 `default` 作为 `deepseek-default` 的别名（issue #99），
但 `models::list()` 只输出 `deepseek-{type}`，客户端拉取模型列表后
仍找不到 `default`。

**现状**：`list()` 同时输出 `deepseek-default` 与 `default`；
`get()` 增加裸名查询分支；别名与 model_type 同名时不重复列出。

### 1.9 `finish_reason` 在非流式响应中可能为 `null`

**规范**：`CreateChatCompletionResponse` 的 `finish_reason` 虽然 nullable，
但实际总是有值。上游 EOF 未给 finish_reason 时，旧实现输出 `null`，
部分客户端会当成异常中断。

**现状**：`aggregate()` 退化为 `"stop"`。

### 1.10 RepairStream 心跳伪造空 tool_call

**问题**：工具调用修复等待期间，心跳 chunk 携带
`tool_calls: [{id: "", function: {name: "", arguments: ""}}]`。
客户端按 `index`/`id` 累积工具调用时会把空调用当成新的工具调用
（与 issue #87 同类症状，只是触发路径在修复分支上）。

**现状**：心跳改为发送**空 delta**（协议合法 no-op），与
`tool_parser.rs` 中已修复的 keepalive 行为一致。

### 1.11 请求解析错误提示重复前缀

**问题**：`BadRequest` 由 `thiserror` 渲染为 `bad request: {0}`，
但 handler 构造时又拼了一次，输出 `bad request: bad request: ...`。

**现状**：handler 改为 `invalid JSON body: {e}`，最终消息为
`bad request: invalid JSON body: ...`。

### 1.12 401 错误信封取值对齐官方（v0.5.1）

**规范/实现**：OpenAI 官方 401 响应为
`{"error":{"message":"Incorrect API key provided: ...","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}`
（`type` 是 `invalid_request_error`，`code` 是 `invalid_api_key`）。

**修复前**：本仓库返回 `type=authentication_error`、`code=invalid_api_token` ——
结构合法但取值与官方不同，按 `code` 分派的网关 / 客户端会归错类。

**现状**：`/v1/*` 的 401 使用官方取值。`/anthropic/*` 仍使用 Anthropic 规范的
`authentication_error`（两套协议的错误语义不同，不可混用）。

### 1.13 `n` 参数未校验（v0.5.1）

**规范**：`CreateChatCompletionRequest.n` 的 minimum 为 1，语义是「返回的候选数」。

**修复前**：`n` 被解析后静默忽略 —— 客户端请求 `n=3` 只会拿到 1 条候选，
却无法察觉（`choices[0]` 之外没有任何提示）。

**现状**：`n=0` 与 `n>1` 都返回 `400 invalid_request_error`
（上游只产出单候选，无法兑现多候选时必须显式报错）。

### 1.14 流式 `finish_reason` 被硬编码为 `stop`（v0.5.1）

**规范**：`finish_reason` 取 `stop` / `length` / `tool_calls` / `content_filter` 等，
其中 `length` 表示「因 `max_tokens` 或上下文上限被截断」。

**修复前**：`ConverterStream` 在收到 `StreamEvent::Done` 时一律下发 `"stop"`，
上游给出的结束原因（例如截断）被丢弃 —— 客户端无法区分「答完了」和「被截断了」。

**现状**：采用 `Done` 事件携带的原因（`length` / `tool_calls`），未知或缺省才退化为 `stop`。

### 1.15 Anthropic 流中途错误缺少 `error` 事件（v0.5.1）

**规范**（docs.anthropic.com/en/api/messages-streaming）：流中可下发
`event: error`，负载为 `{"type":"error","error":{"type":"<kind>","message":"..."}}`，
SDK 会据此抛出对应异常。

**修复前**：上游在 `message_start` 之后中断时，只补发 `message_delta` + `message_stop` ——
客户端把**截断**当成正常结束（用户看到半截答案，且不会再重试）。

**现状**：先补发 `error` 事件（`overloaded_error` / `invalid_request_error` / `api_error`），
再补收尾事件；与 Responses 适配层在同样场景下发 `response.failed` 的行为一致。

### 1.16 缺少 `x-request-id` 响应头（v0.5.1）

**实现**：官方 API 的每个响应都带 `x-request-id`，便于客户端把一次调用与服务端日志对上。

**现状**：最外层中间件为每个请求生成 `req-{n}`，写入请求扩展（handler 复用同一个 ID 打日志）
并回填到**所有**响应（含 401 等中间件产生的响应）。

---

## 2. 已确认符合规范、无需修改

| 项目 | 结论 |
|------|------|
| 流式终止符 `data: [DONE]` | 正确。`openai-python` `_streaming.py` 显式跳过 `[DONE]`，且要求存在（否则 `SSEDecoder` 迭代提前结束） |
| `chat.completion.chunk` / `chat.completion` 的 `object` 取值 | 正确 |
| `finish_reason` 取值集合 | 仅输出 `stop` / `tool_calls`，均在规范枚举内 |
| `usage` 字段名 `prompt_tokens` / `completion_tokens` / `total_tokens` | 正确 |
| `tool_calls[].index/id/type/function.name/function.arguments` | 正确，`arguments` 始终为 JSON 字符串 |
| SSE 帧格式 `data: {...}\n\n` | 正确（`Content-Type: text/event-stream` + `Cache-Control: no-cache`） |
| Anthropic SSE 帧格式 `event: <name>\ndata: <json>\n\n` | 正确 |
| Anthropic 流**不**应以 `[DONE]` 结尾 | 正确，本项目不发 `[DONE]`；`anthropic-sdk-python` 以 `message_stop` 结束迭代 |
| Anthropic `ping` 事件 | 正确，且是 SDK 明确处理的事件（`_streaming.py` 中 `if sse.event == "ping"`） |
| Anthropic `message_delta.usage` 只带 `output_tokens` | 正确（规范中该字段为必填，其余可选） |
| Anthropic `message_start` 中 `stop_reason` 为 `null` | 正确（规范明确流式下 message_start 为 null） |

---

## 3. Responses API（新增）

`/v1/responses` 的字段名、事件名与事件负载已逐条对照 `CreateResponse`
与 `ResponseStreamEvent` schema 实现，详见
[`responses-api.md`](./responses-api.md)。

关键差异点（相对 Chat Completions，适配层已抹平）：

| 维度 | Chat Completions | Responses |
|------|------------------|-----------|
| usage 字段名 | `prompt_tokens` / `completion_tokens` | `input_tokens` / `output_tokens` |
| usage 必填子对象 | 可选 | `input_tokens_details.cached_tokens`、`output_tokens_details.reasoning_tokens` 均为 required |
| 工具定义结构 | `{type, function:{name, parameters}}` | `{type, name, parameters}`（扁平） |
| 工具调用表示 | `message.tool_calls[]` | 独立的 `function_call` output item（含 `call_id`） |
| 多轮上下文 | 客户端重放 messages | `previous_response_id` |
| 流式事件 | `choices[].delta` | 每种事件独立 `type` + `sequence_number` |
| SSE `event:` 名 | 无（仅 `data:`） | 有，且与 JSON 内 `type` 同名 |

---

## 4. 尚未实现（有意为之）

以下能力**未**实现，且已确认不影响主流客户端（OpenAI SDK / Codex CLI /
Agents SDK 的基本用法）：

| 能力 | 说明 |
|------|------|
| `n > 1` | DeepSeek 上游不支持多候选；参数被解析但忽略 |
| `logprobs` / `top_logprobs` | 上游不返回；字段恒为 `null` |
| `logit_bias` / `seed` | 上游不支持 |
| `audio` / `modalities` 输出 | 上游无音频输出能力 |
| Responses 内置工具执行（`web_search_call` / `file_search_call` / `code_interpreter_call` / `mcp_call`） | 上游不提供；`web_search_preview` 会退化为 DeepSeek 的搜索模式，但不产出对应的 output item |
| `max_tokens` / `max_completion_tokens` 的**强制截断** | 上游 completion 载荷没有对应字段（真实客户端也不发送），因此无法强制；参数被解析但不生效。上游自身因上下文上限中断时会以 `finish_reason=length` 反映（见 1.14） |
| `response.reasoning_summary_text.delta` 的 `summary_part` 细分 | 只产出单一段落（`summary_index: 0`） |
| `store` 的跨进程持久化 | 使用进程内 TTL 缓存，见 [`responses-api.md`](./responses-api.md#previous_response_id-的取舍) |
| `/v1/responses/input_tokens`、`background` 模式 | 未实现路由，返回 404 |
| `/v1/responses/{id}` 查询 | **已实现**（有界 + TTL 的进程内缓存；重启即失效，磁盘持久化待排期） |

---

## 5. 客户端互操作矩阵

| 客户端 | 使用端点 | 依赖的关键行为 | 状态 |
|--------|----------|----------------|------|
| `openai` Python/Node SDK（chat.completions） | `/v1/chat/completions` | role chunk、`[DONE]`、`finish_reason`、`include_usage` | ✅ |
| `openai` Python/Node SDK（responses） | `/v1/responses` | `response.created` → `response.output_text.delta` → `response.completed` | ✅ |
| Anthropic SDK / Claude Code | `/anthropic/v1/messages` | `x-api-key`、`content_block_*` 序列、`message_stop`、Anthropic 错误信封 | ✅ |
| Codex CLI | `/v1/responses` | `function_call` output item、`previous_response_id`、`response.completed.usage` | ✅（需 `store` 未禁用） |
| OpenAI Agents SDK | `/v1/responses` | 同上 + `response.output_item.done` | ✅ |
| LiteLLM `/v1/responses` 桥 | `/v1/responses` | 事件名与 JSON 内 `type` 一致 | ✅ |
| Continue / Cline / Roo Code | `/v1/chat/completions` | 标准 tool_calls delta | ✅ |

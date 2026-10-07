# OpenAI Responses API 支持说明

本代理实现 `POST /v1/responses`（OpenAI Responses API），在
`openai_adapter` 之上做纯协议翻译，不直接访问 `ds_core`。

规范基准：`openai-openapi` 的 `CreateResponse` / `InputItem` / `OutputItem` /
`ResponseStreamEvent` schema，以及 `openai-python` 的
`lib/streaming/responses/_responses.py`（事件累积逻辑）。

---

## 模块结构

```
src/responses_adapter.rs            门面：ResponsesAdapter、错误映射、配置注入
src/responses_adapter/
├── types.rs    协议类型：请求、Response 对象、usage、SSE 序列化
├── request.rs  ResponsesRequest → ChatCompletionsRequest 映射
├── response.rs Response 对象构造 + SSE 事件状态机（含 stream/store 钩子）
└── store.rs    previous_response_id 的进程内 TTL 缓存
```

数据流：

```
ResponsesRequest ──request.rs──▶ ChatCompletionsRequest
                                       │
                                OpenAIAdapter::chat_completions()
                                       │
            ChatOutput::Stream ──response::stream──▶ Responses SSE 事件
            ChatOutput::Json   ──response::from_chat_completions──▶ Response 对象
```

---

## 请求支持情况

### 已消费

| 字段 | 映射 / 行为 |
|------|-------------|
| `model` | 与 Chat Completions 共用 registry（接受 `deepseek-default` 与裸 `default`） |
| `input` | 字符串 → 单条 user 消息；数组 → 输入项序列（见下） |
| `instructions` | 前置 system 消息；字符串与 `input_text` 数组两种形态都支持 |
| `stream` | 分流 SSE / JSON |
| `max_output_tokens` | → `max_tokens` |
| `temperature` / `top_p` | 透传 |
| `tools` | 支持扁平 Responses 结构与嵌套 Chat Completions 结构；`web_search_preview` 触发搜索模式 |
| `tool_choice` | `auto`/`none`/`required`、`{type:"function",name}`、`{type:"custom",name}`、`{type:"allowed_tools",mode,tools}` |
| `parallel_tool_calls` | 透传 |
| `reasoning.effort` | → `reasoning_effort`；缺省时由 adapter 默认 `"high"` 接管 |
| `text.format` | `json_object` / `json_schema` → `response_format`；`text` 不注入约束 |
| `store` | 控制是否写入 `previous_response_id` 缓存（默认 `true`） |
| `previous_response_id` | 从缓存恢复历史消息 |
| `metadata` / `user` | 透传并回显 |
| `stream_options.include_obfuscation` | 控制 `ChatCompletionsResponseChunk.obfuscation` |

### 已解析但不消费

`background`、`include`、`max_tool_calls`、`prompt`、`prompt_cache_key`、
`prompt_cache_retention`、`safety_identifier`、`service_tier`、`top_logprobs`、
`truncation`。这些字段在 `types.rs` 中保留（供后续扩展），当前静默忽略，
不影响客户端解析响应。

### `input` 输入项类型

| `type` | 处理 |
|--------|------|
| （省略，含 `role`）或 `message` | role=`developer` 降级为 `system`；内容支持字符串与内容块数组 |
| `function_call` | 合并到前一条 assistant 消息的 `tool_calls`（没有则新建） |
| `function_call_output` | 转为 `role: "tool"` 消息，携带 `tool_call_id` |
| `item_reference` | 忽略（无跨响应 item 存储） |
| `reasoning` / 服务器工具项 | 忽略 |

内容块：`input_text` / `output_text` / `text` → 文本；
`input_image` / `image_url` → 图片（URL 触发搜索模式）；
`input_file` → 文件上传（data URL）。纯文本块会合并为单个字符串，
只有存在非文本块时才使用多模态 parts 形态。

---

## 响应对象

```jsonc
{
  "id": "resp_...",
  "object": "response",
  "created_at": 1741290958,
  "completed_at": 1741290959,
  "status": "completed",        // completed | incomplete | failed | in_progress
  "error": null,
  "incomplete_details": null,   // {"reason":"max_output_tokens"} | {"reason":"content_filter"}
  "instructions": "You are a helpful assistant.",
  "max_output_tokens": null,
  "model": "deepseek-default",
  "output": [ /* message | reasoning | function_call */ ],
  "output_text": "...",         // SDK 便利字段，聚合所有 output_text
  "parallel_tool_calls": true,
  "previous_response_id": null,
  "reasoning": { "effort": null, "summary": null },
  "store": true,
  "temperature": 1.0,
  "text": { "format": { "type": "text" } },
  "tool_choice": "auto",
  "tools": [],
  "top_p": 1.0,
  "truncation": "disabled",
  "usage": {
    "input_tokens": 37,
    "input_tokens_details": { "cached_tokens": 0 },
    "output_tokens": 11,
    "output_tokens_details": { "reasoning_tokens": 0 },
    "total_tokens": 48
  },
  "user": null,
  "metadata": {}
}
```

### output item 形态

| 类型 | 关键字段 |
|------|----------|
| `reasoning` | `id`、`summary: [{type:"summary_text", text}]`、`status` |
| `message` | `id`、`status`、`role:"assistant"`、`content:[{type:"output_text", text, annotations, logprobs}]` |
| `function_call` | `id`、`call_id`、`name`、`arguments`（JSON **字符串**）、`status` |

`output` 的顺序：有推理时 `reasoning` 在前，随后 `function_call`，
最后 `message`。仅当存在文本（或完全没有工具调用）时才产出 `message` 项 ——
与 OpenAI 一致。

`status` 推导规则：

| 上游 `finish_reason` | Response `status` | `incomplete_details.reason` |
|----------------------|-------------------|------------------------------|
| `stop` / `tool_calls` | `completed` | `null` |
| `length` | `incomplete` | `max_output_tokens` |
| `content_filter` | `incomplete` | `content_filter` |
| 上游中途出错 | `failed`（附 `error`） | — |

---

## 流式事件

### 事件序列

```text
event: response.created
event: response.in_progress
event: response.output_item.added             # reasoning / message / function_call
event: response.reasoning_summary_part.added  # 仅 reasoning 项
event: response.reasoning_summary_text.delta  # 仅 reasoning 项
event: response.content_part.added            # 仅 message 项
event: response.output_text.delta             # 仅 message 项
event: response.function_call_arguments.delta # 仅 function_call 项
event: response.function_call_arguments.done  # 仅 function_call 项
event: response.output_text.done
event: response.content_part.done
event: response.reasoning_summary_text.done
event: response.reasoning_summary_part.done
event: response.output_item.done
event: response.completed                     # 或 response.incomplete / response.failed
data: [DONE]
```

每个事件的 JSON 都同时带：
- `type`：与 SSE `event:` 名完全一致（LiteLLM 等网关按 JSON 内的 `type` 分派）
- `sequence_number`：从 0 起严格递增，无空洞、不重复

### 与 OpenAI 的行为差异（有意为之）

| 差异 | 原因 |
|------|------|
| usage 到达前不发出 `response.completed` | 上游可能把 usage 放在 finish 之后的独立 chunk 里；先发会让客户端拿到 `usage: null` 的终态。若上游在 EOF 前一直没给 usage，则在 EOF 时以 `null` 收尾 |
| `response.reasoning_summary_text.done` 的 `summary_index` 恒为 0 | 上游只有一整段 reasoning，不做分段 |
| `response.failed` 之后仍会发 `data: [DONE]` | 保证客户端流迭代正常终止，避免 read timeout |
| 不产出 `response.output_text.annotation.added` | 上游的搜索引用不作为 annotation 暴露 |

### 错误处理

**响应开始前**出错（鉴权、参数校验、模型不存在、账号池不可用）：返回普通
HTTP 错误 + OpenAI 错误信封，状态码按 `OpenAIAdapterError::status_code()`。

**响应开始后**出错（上游流中断）：SSE 已返回 200，无法改状态码，
因此发出 `response.failed` 事件（含 `response.error.{code,message}`），
随后 `data: [DONE]`。这是协议内的正确做法 —— 与 `response.completed`
互斥。

---

## `previous_response_id` 的取舍

OpenAI 会服务端保存响应（默认 30 天），`previous_response_id` 直接引用即可。
本代理是**无状态网关**，无法做等价的长期存储，但完全不支持会让
Codex CLI / OpenAI Agents SDK 每轮丢失上下文。因此实现
**进程内、有界、带 TTL** 的缓存：

```toml
[ds_core]
responses_store_capacity = 256   # 最多保存多少轮（默认 256）
responses_store_ttl_secs = 3600  # 条目存活秒数（默认 3600）
```

行为：

- 仅在请求未显式设置 `"store": false` 时写入
- 保存内容为**重建上下文所需的最小信息**：上一轮的用户输入 + output 数组
- 超过容量时按插入顺序淘汰最旧条目；超过 TTL 后读取即视为不存在
- 引用未知/已过期 ID 时返回 **400**（`invalid_request_error`），
  错误信息提示客户端重放完整 `input`

**升级/重启后缓存清空**。需要跨重启稳定上下文的客户端应每轮重放完整
`input`，或自行持久化 `output` 数组。

缓存按值克隆（`ResponseStore` 内部是 `Arc`），流式响应的收尾钩子与
请求处理路径共享同一份缓存。

---

## 未实现

| 能力 | 状态 |
|------|------|
| `GET /v1/responses/{id}` | **已实现**：返回创建时保存的完整 Response 对象快照；不存在 / 被容量淘汰 / 超过 TTL → 404（issue #110 第一步） |
| `POST /v1/responses/{id}/cancel` | 未实现路由（404） |
| `POST /v1/responses/input_tokens` | 未实现路由（404） |
| `background: true` | 忽略，始终同步返回 |
| `include` 的附加内容 | 忽略 |
| 内置工具（`web_search_call` / `file_search_call` / `code_interpreter_call` / `mcp_call`） | 不产出对应 output item；`web_search_preview` 仅退化为 DeepSeek 搜索模式 |
| `reasoning.encrypted_content` | 不产出；跨轮推理上下文不保留 |
| `truncation: "auto"` | 忽略；超长输入走既有的 oversized 回退（历史切分 / 分块） |

---

## 验证

单元测试（`cargo test responses_adapter`）覆盖：

- 请求映射：字符串/数组 input、instructions 两种形态、developer 角色降级、
  `function_call` 折叠为 assistant tool_calls、工具扁平/嵌套两种结构、
  `tool_choice` 各变体、`text.format`、`reasoning.effort`
- 事件序列：完整顺序、`sequence_number` 单调、usage 延迟发出、
  `[DONE]` 终止符、`length → incomplete`、上游错误 → `response.failed`、
  EOF 无 finish_reason 仍能收尾
- 缓存：容量淘汰、TTL 过期、克隆共享状态

端到端（需真实账号）：

```bash
just e2e-serve                                      # 终端 A
just e2e-responses --show-output                    # 终端 B
```

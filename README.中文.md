# DeepSeek Web API

[![](https://img.shields.io/github/license/NIyueeE/deepseek-web-api.svg)](LICENSE)
![](https://img.shields.io/github/stars/NIyueeE/deepseek-web-api.svg)
![](https://img.shields.io/github/forks/NIyueeE/deepseek-web-api.svg)

[English](./README.md) | [中文](./README.中文.md)

受 [deepseek2api](https://github.com/iidamie/deepseek2api) 启发。DeepSeek 网页端 API 的透明代理，提供自动认证和 PoW 计算。

## 特性

- **自动认证**: 服务端管理账号凭据，客户端无需认证（令牌在首次API调用时获取）
- **PoW (工作量证明)**: 自动解决 PoW 挑战
- **SSE 流式响应**: 透传 DeepSeek 的 SSE 响应
- **OpenAI 兼容 API**: `/v1/chat/completions` 端点，完整工具调用支持

## 快速开始

**初次使用？** 请参照[新手友好的安装指南](https://./docs/INSTALL.md)获取分步说明（仅需 `uv`）。

```bash
# 配置账号
cp config.toml.example config.toml
# 编辑 config.toml 填入 DeepSeek 账户密码

# 运行服务
uv run python main.py
```

**注意**：仅支持单用户模式，以防止对 DeepSeek 服务器造成过多负载。~不会实现多用户请求。~

## 配置

运行前需要配置 `config.toml`：

```toml
[server]
host = "127.0.0.1"                  # 建议保持 loopback，仅本机访问
port = 5001
reload = true
cors_origins = ["*"]                 # 建议改成明确白名单
cors_allow_credentials = false
cors_allow_methods = ["*"]
cors_allow_headers = ["*"]
pool_size = 10                       # 最大并发 DeepSeek session 数；超出时请求等待，超时返回 503
pool_acquire_timeout = 30.0          # 等待可用 session 的超时秒数，超时返回 503

[auth]
tokens = []                          # 配置一个或多个 token 启用鉴权
# 示例: tokens = ["sk-prod-xxx", "sk-backup-yyy"]

[account]
email = "your_email@example.com"   # 邮箱登录（优先）
mobile = ""                        # 手机号登录（email 为空时使用）
area_code = "86"                   # 手机号区号，如 "86"
password = "your_password"
token = ""                         # 非必须，系统会自动管理（首次使用后保存）
```

**Docker / 环境变量预留**:
- `CONFIG_PATH`: 配置文件路径（env: `CONFIG_PATH`，默认 `config.toml`）

**WASM 模块**：通过 `config.toml` 的 `[wasm]` section 配置：
- `url`: 下载地址（首次运行自动下载）
- `path`: 本地保存路径（默认 `core/deepseek.wasm`）

**安全提示**：
- `[auth].tokens` 是简单的字符串数组。非空数组表示需要鉴权；空数组表示匿名访问（仅在 loopback 时安全）。
- 只要配置了至少一个 token，所有 `/v0/*` 和 `/v1/*` 请求都必须携带 `Authorization: Bearer <token>` 或 `X-API-Key: <token>`。
- **Fail-fast 保护**: 如果 `[server].host` 是非 loopback（如 `0.0.0.0`）且 `[auth].tokens` 为空，服务将拒绝启动。
- CORS 可通过 `[server].cors_*` 配置；为了兼容旧行为，默认仍较宽松，但面向浏览器暴露时应收紧 `cors_origins`。
- 即便如此，仍建议只监听 `127.0.0.1`（`main.py` 默认值）。

## 模型

通过 `/v1/models` 可用的模型：

| 模型 | 说明 |
|------|------|
| `deepseek-web-chat` | 标准对话模型，禁用思考 |
| `deepseek-web-reasoner` | 推理模型，支持思维链 |

**注意**：默认禁用内部搜索功能。

## 使用案例

[AstrBot](https://github.com/AstrBotDevs/AstrBot) 集成示例，流式思考和工具调用正常工作：

![AstrBot with deepseek-web-reasoner](./assets/reasoner-show.png)

## API 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/v1/chat/completions` | POST | OpenAI兼容对话接口，支持工具调用 |
| `/v0/chat/completion` | POST | 发送对话，透传 SSE |
| `/v0/chat/create_session` | POST | 创建新会话 |
| `/v0/chat/delete` | POST | 删除会话 |
| `/v0/chat/history_messages` | GET | 获取聊天历史 |
| `/v0/chat/upload_file` | POST | 上传文件 |
| `/v0/chat/fetch_files` | GET | 查询文件状态 |
| `/v0/chat/message` | POST | 编辑消息 |

### 端点详情

#### POST /v1/chat/completions
OpenAI兼容的对话完成接口，完全支持工具调用和流式响应。完全兼容 OpenAI SDK。

**功能**:
- 接受 OpenAI 风格的 `messages` 数组
- 支持 `tool_calls` 和多轮工具对话
- 流式/非流式响应
- 内部使用 `edit_message` API 无状态会话

**支持的 OpenAI 参数**：

| 参数 | 类型 | 说明 |
|------|------|------|
| `model` | string | 模型 ID，默认为 `deepseek-web-chat` |
| `messages` | array | OpenAI 风格消息数组 |
| `stream` | bool | 流式响应，默认为 `false` |
| `tools` | array | 函数调用工具定义 |
| `tool_choice` | string \| object | 控制模型可以调用哪些工具。值：`"auto"`（默认）、`"none"`（禁用工具）、`"required"`（必须至少调用一个工具）、或 `{"type": "function", "function": {"name": "..."}}`（调用指定工具）。此参数仅在代理层使用，不会转发给 DeepSeek。 |
| `parallel_tool_calls` | bool | 是否允许并行工具调用。默认为 `true`。设为 `false` 时，模型将被指示每次只调用一个工具。此参数仅在代理层使用，不会转发给 DeepSeek。 |
| `extra_body` | dict | DeepSeek 特有参数（见下方） |

> **关于 `tools` 的说明**：每个工具在 `function` 对象内支持 `strict` 属性（如 `{"type": "function", "function": {"name": "...", "strict": true}}`）。当 `strict: true` 时，模型将被指示严格遵循 JSON Schema——不添加未定义的字段、不省略必填字段、不使用枚举列表以外的值。提示中会同时包含自然语言描述和 JSON Schema 代码块，以获得最大的约束保真度。

**通过 `extra_body` 传递 DeepSeek 特有参数**：

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `search_enabled` | bool | `false` | 启用 DeepSeek 网页后端搜索功能 |
| `thinking_enabled` | bool | `true`（reasoner模型），`false`（chat模型） | 开启思考。对 `deepseek-web-chat`：设为 `true` 开启思考输出；对 `deepseek-web-reasoner`：设为 `false` 禁用思考 |

OpenAI SDK 示例：
```python
from openai import OpenAI

client = OpenAI(api_key="your-token", base_url="http://localhost:5001/v1")
response = client.chat.completions.create(
    model="deepseek-web-chat",
    messages=[{"role": "user", "content": "你好"}],
    extra_body={
        "search_enabled": True,      # 启用 DeepSeek 网页搜索
        "thinking_enabled": True,     # 开启思考输出（对 chat 模型有效）
    }
)
```

### 端点详情

#### POST /v0/chat/completion
**外部表现**: 接收 `prompt`、可选 `chat_session_id`，返回 SSE 流。

**内部操作**:
- 无 `chat_session_id` → 通过 `POST /api/v0/chat_session/create` 创建会话，本地存储，返回 `X-Chat-Session-Id` header
- 有 `chat_session_id` → 从本地存储查找 `parent_message_id`，附加到请求
- 添加 `Authorization`、`x-ds-pow-response` headers，转发至 DeepSeek
- 解析 SSE 提取 `response_message_id`，更新本地会话存储

#### POST /v0/chat/create_session
**外部表现**: 接收 `{"agent": "chat"}`，返回 DeepSeek 会话数据。

**内部操作**:
- 转发至 `POST /api/v0/chat_session/create`
- 从响应中提取 `chat_session_id`，存入本地会话映射
- 返回 DeepSeek 响应，并在顶层显式添加 `chat_session_id` 字段

#### POST /v0/chat/delete
**外部表现**: 接收 `{"chat_session_id": "..."}`，返回 DeepSeek 响应。

**内部操作**:
- 从本地会话存储删除会话
- 转发至 `POST /api/v0/chat_session/delete`

#### GET /v0/chat/history_messages
**外部表现**: 查询参数 `chat_session_id`、`offset`、`limit`，返回消息历史。

**内部操作**:
- 添加 `Authorization` header，转发至 `GET /api/v0/chat/history_messages`

#### POST /v0/chat/upload_file
**外部表现**: Multipart 表单，包含 `file` 字段，返回 DeepSeek 响应。

**内部操作**:
- 从表单读取文件，转发至 `POST /api/v0/file/upload_file`

#### GET /v0/chat/fetch_files
**外部表现**: 查询参数 `file_ids`（逗号分隔），返回文件状态。

**内部操作**:
- 添加 `Authorization` header，转发至 `GET /api/v0/file/fetch_files`

详见 [v0_API](./docs/v0_API.md)。

## 实现说明

### OpenAI 适配器 (`/v1/chat/completions`)
通过 `edit_message` API 实现无状态会话：
- 客户端传入完整的 `messages` 数组，适配器将对话历史注入提示词
- 使用 `message_id=1` 的 `edit_message` 固定编辑最新用户消息
- 模型始终认为这是"第一次对话"，避免会话状态累积
- 支持 `deepseek-web-reasoner` 模型的思考内容

**Session Pool**：
- 维护有上限的 DeepSeek session 池（`pool_size`，默认 10）
- 当所有 session 繁忙时，新请求最多等待 `pool_acquire_timeout` 秒（默认 30s），超时返回 HTTP 503
- 空闲 session 每 `max_idle_seconds/2`（默认 150s）自动清理
- 硬上限防止并发请求时无限制地创建 session 触发限流

**限流处理**：
- `proxy_to_deepseek_stream` 在 yield 任何字节前检测 HTTP 429 和 5xx 响应
- 遇到限流时最多重试 3 次，指数退避（5s、10s、20s），并遵守 `Retry-After` header
- 每次重试独立获取新 PoW（旧 PoW 会过期）
- 全部重试失败后：流式返回 SSE 错误 chunk；非流式返回 HTTP 503
- 限流错误不会触发 session pool 重试（账号级限流，换 session 无效）

**防幻觉机制**：
当模型输出 `[TOOL🛠️]...[/TOOL🛠️]` 时：
1. 适配器提取并解析工具调用 JSON
2. 向客户端发送 `tool_calls` chunk 和 `finish_reason=tool_calls`
3. 发送 `data: [DONE]\n\n` 通知流结束
4. 继续消费 DeepSeek 剩余流（丢弃数据），以正确关闭连接

## TODO

- [x] 简单包装 deepseek_web_chat API
- [x] 实现 openai_chat_completions 协议适配器
- [x] openai适配器的流式工具调用提取
- [x] Session pool 硬上限，防止并发 session 创建触发限流
- [x] 流式路径限流检测（HTTP 429/5xx）与指数退避重试
- [ ] 通过 [litellm](https://github.com/BerriAI/litellm) 实现 claude_message 协议适配器（转换OpenAI协议到Claude协议）
- [ ] 实现多用户账户负载均衡，防止 DeepSeek 请求频率限制

## 架构

```mermaid
flowchart LR
    Client["客户端<br/>(OpenAI SDK / curl)"]

    subgraph v1["/v1/chat/completions"]
        direction TB
        pool["StatelessSession<br/>Pool"]
        generator["stream_generator<br/>(SSE格式转换)"]
    end

    subgraph v0["/v0/chat/*"]
        direction TB
        store["ParentMsgStore<br/>(chat_session_id →<br/>parent_message_id)"]
        service["v0_service.py<br/>stream_chat_completion<br/>stream_edit_message"]
    end

    subgraph core["核心模块"]
        auth["auth.py<br/>(认证)"]
        pow["pow.py<br/>(PoW)"]
    end

    Client --> v1
    Client --> v0
    pool --> generator
    store --> service
    v1 --> core
    v0 --> core
    core --> DeepSeek["DeepSeek 后端"]
```

## 免责声明

DeepSeek 官方 API 非常便宜，请大家多多支持官方服务。

本项目的初心是想体验官方网页端灰度测试的最新模型。

**严禁商用**，避免对官方服务器造成压力，否则风险自担。

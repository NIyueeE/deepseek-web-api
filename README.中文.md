# DeepSeek Web API

[English](./README.md) | [中文](./README.中文.md)

受 [deepseek2api](https://github.com/iidamie/deepseek2api) 启发。DeepSeek Chat API 的透明代理，提供自动认证和 PoW 计算。

## 特性

- **自动认证**: 服务端管理账号凭据，客户端无需认证
- **PoW (工作量证明)**: 自动解决 PoW 挑战
- **会话管理**: 通过 `chat_session_id` 支持多轮对话
- **SSE 流式响应**: 透传 DeepSeek 的 SSE 响应
- **OpenAI 兼容 API**: `/v1/chat/completions` 端点，完整工具调用支持
- **流式工具调用**: 提取并转换 `[TOOL🛠️]...[/TOOL🛠️]` 标记为 OpenAI `delta.tool_calls` 格式

## 快速开始

```bash
# 配置账号
cp config.toml.example config.toml
# 编辑 config.toml 填入 DeepSeek 凭据

# 运行服务
uv run python main.py
```

**注意**：仅支持单用户模式，以防止对 DeepSeek 服务器造成过多负载。不会实现多用户请求。

## 配置

运行前需要配置 `config.toml`：

```toml
[account]
email = "your_email@example.com"      # DeepSeek 账号邮箱
password = "your_password"           # DeepSeek 账号密码
token = "your_deepseek_token"       # DeepSeek 认证令牌（提供邮箱/密码时可省略）
```

**安全提示**：`/v1/chat/completions` 端点没有 API Token 验证。**请务必将服务运行在 `127.0.0.1`**（`main.py` 默认值），以防止未授权访问。

## 模型

通过 `/v1/models` 可用的模型：

| 模型 | 说明 |
|------|------|
| `deepseek-web-chat` | 标准对话模型，禁用思考 |
| `deepseek-web-reasoner` | 推理模型，支持思维链 |

**注意**：默认禁用内部搜索功能（无网页搜索）。

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

### 端点详情

#### POST /v1/chat/completions
OpenAI兼容的对话完成接口，支持完整的工具调用功能。

**功能**:
- 接受OpenAI风格的 `messages` 数组，支持角色：`system`、`user`、`assistant`、`tool`
- 支持 `tool_calls` 在助手消息中，用于多轮工具对话
- 工具结果作为 `role: "tool"` 传递，包含 `tool_call_id` 和 `content`
- 流式响应：提取 `[TOOL🛠️]...[/TOOL🛠️]` 标记并转换为 `delta.tool_calls` 数据块
- 非流式响应：从完整响应文本中提取工具调用
- 基于模型的行为：`deepseek-web-reasoner` 启用思考/推理内容

**请求体**:
```json
{
  "model": "deepseek-web-reasoner",
  "messages": [
    {"role": "user", "content": "天气怎么样？"}
  ],
  "stream": false,
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "获取城市天气",
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

**响应** (含工具调用):
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
        "function": {"name": "get_weather", "arguments": "{\"city\": \"北京\"}"}
      }]
    },
    "finish_reason": "tool_calls"
  }]
}
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

详见 [API.md](./API.md)。

## 实现说明

### OpenAI 适配器 (`/v1/chat/completions`)
OpenAI兼容适配器通过向内部 `/v0/chat/completion` 端点注入提示词来工作：
- 将 OpenAI 的 `messages` 数组转换为带角色标记的提示词格式
- 将工具schema注入系统指令，说明使用 `[TOOL🛠️]...[/TOOL🛠️]` 格式回复
- 实时解析流式SSE响应，提取工具调用标记
- 通过将工具结果作为 `[TOOL_RESULT]` 标记传回来支持多轮对话

## TODO

- [x] 简单包装 deepseek_web_chat API
- [x] 实现 openai_chat_completions 协议适配器
- [x] openai适配器的流式工具调用提取
- [ ] 通过 [litellm](https://github.com/BerriAI/litellm) 实现 claude_message 协议适配器（转换OpenAI协议到Claude协议）

## 架构

```
客户端 --> DeepSeek Web API --> DeepSeek 后端
              |
              +-- 认证管理（自动）
              +-- PoW 求解
              +-- 会话状态管理
```

## 免责声明

DeepSeek 官方 API 非常便宜，请大家多多支持官方服务。

本项目的初心是想体验官方网页端灰度测试的最新模型。

**严禁商用**，避免对官方服务器造成压力，否则风险自担。

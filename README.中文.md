# DeepSeek Web API

[English](./README.md) | [中文](./README.中文.md)

DeepSeek Chat API 的透明代理，提供自动认证和 PoW 计算。

## 特性

- **自动认证**: 服务端管理账号凭据，客户端无需认证
- **PoW (工作量证明)**: 自动解决 PoW 挑战
- **会话管理**: 通过 `chat_session_id` 支持多轮对话
- **SSE 流式响应**: 透传 DeepSeek 的 SSE 响应

## 快速开始

```bash
# 配置账号
cp config.toml.example config.toml
# 编辑 config.toml 填入 DeepSeek 凭据

# 运行服务
uv run python main.py
```

## API 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/v0/chat/completion` | POST | 发送对话，透传 SSE |
| `/v0/chat/create_session` | POST | 创建新会话 |
| `/v0/chat/delete` | POST | 删除会话 |
| `/v0/chat/history_messages` | GET | 获取聊天历史 |
| `/v0/chat/upload_file` | POST | 上传文件 |
| `/v0/chat/fetch_files` | GET | 查询文件状态 |

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

## TODO

- [x] 简单包装 deepseek_web_chat API
- [ ] 实现 claude_message 协议代理
- [ ] 实现 openai_chat_completions 协议代理

## 架构

```
客户端 --> DeepSeek Web API --> DeepSeek 后端
              |
              +-- 认证管理（自动）
              +-- PoW 求解
              +-- 会话状态管理
```

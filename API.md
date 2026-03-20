# DeepSeek Web API 文档

## 概述

本服务是 DeepSeek Chat API 的透明代理，提供自动认证和 PoW 计算。

**Base URL**: `http://localhost:5001/v0/chat`

**设计原则**: 透传 + 最小包装，只在必要时添加认证和 PoW。

---

## 端点列表

| 端点 | 方法 | 说明 |
|------|------|------|
| `/completion` | POST | 发送对话，透传 SSE |
| `/delete` | POST | 删除 session |
| `/upload_file` | POST | 上传文件 |
| `/fetch_files` | GET | 查询文件状态 |
| `/history_messages` | GET | 获取历史消息 |
| `/create_session` | POST | 创建新 session |

---

## 1. POST /completion

发送对话请求，自动管理会话状态。

### 请求体

```json
{
  "prompt": "你好",
  "chat_session_id": "可选，用于多轮对话",
  "search_enabled": true,
  "thinking_enabled": true,
  "ref_file_ids": []
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `prompt` | string | 是 | 对话内容 |
| `chat_session_id` | string | 否 | 会话 ID，不提供则自动创建 |
| `search_enabled` | boolean | 否 | 是否启用搜索，默认 true |
| `thinking_enabled` | boolean | 否 | 是否启用思考，默认 true |
| `ref_file_ids` | array | 否 | 引用文件 ID 列表 |

### 响应

透传 DeepSeek SSE 流。

### 包装逻辑

1. **无 `chat_session_id`**：调用 `POST /api/v0/chat_session/create` 创建 session，记录到本地映射表
2. **有 `chat_session_id`**：从本地映射表获取 `parent_message_id`，添加到请求体
3. **添加 Header**：Authorization、Bearer Token、x-ds-pow-response

### 发送至 DeepSeek 的 payload

```json
{
  "chat_session_id": "xxx",
  "parent_message_id": null 或 2 或 4...",
  "preempt": false,
  "prompt": "你好",
  "ref_file_ids": [],
  "search_enabled": true,
  "thinking_enabled": true
}
```

---

## 2. POST /delete

删除指定的 session。

### 请求体

```json
{
  "chat_session_id": "需要删除的 session ID"
}
```

### 响应

透传 DeepSeek 响应。

### 包装逻辑

1. 从本地映射表删除记录
2. 转发请求至 `POST /api/v0/chat_session/delete`

---

## 3. POST /upload_file

上传文件到 DeepSeek。

### 请求体

`Content-Type: multipart/form-data`

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `file` | binary | 是 | 文件内容 |

### 响应

透传 DeepSeek 响应。

### 包装逻辑

添加 Authorization header，转发至 `POST /api/v0/file/upload_file`

---

## 4. GET /fetch_files

查询文件解析状态。

### Query 参数

```
GET /fetch_files?file_ids=file-xxx,file-yyy
```

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `file_ids` | string | 是 | 逗号分隔的文件 ID 列表 |

### 响应

透传 DeepSeek 响应。

### 包装逻辑

添加 Authorization header，转发至 `GET /api/v0/file/fetch_files`

---

## 5. GET /history_messages

获取会话的历史消息。

### Query 参数

```
GET /history_messages?chat_session_id=xxx&offset=0&limit=20
```

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `chat_session_id` | string | 是 | 会话 ID |
| `offset` | integer | 否 | 消息偏移，默认 0 |
| `limit` | integer | 否 | 消息数量，默认 20 |

### 响应

透传 DeepSeek 响应。

### 包装逻辑

添加 Authorization header，转发至 `GET /api/v0/chat/history_messages`

---

## 6. POST /create_session

手动创建新 session。

### 请求体

```json
{
  "agent": "chat"
}
```

### 响应

透传 DeepSeek 响应。

### 包装逻辑

添加 Authorization header，转发至 `POST /api/v0/chat_session/create`

---

## 会话状态管理

服务端维护内部映射表：

```
chat_session_id → last_response_message_id
```

### 规则

1. 首次请求（无 `chat_session_id`）：`parent_message_id = null`
2. 后续请求：自动获取上次的 `response_message_id`（通过 SSE 解析）作为 `parent_message_id`

---

## 认证

服务端使用配置的单一账号自动认证，无需客户端提供 Authorization header。

---

## DeepSeek 原始端点映射

| 本服务 | DeepSeek 原始端点 |
|--------|------------------|
| `/completion` | `/api/v0/chat/completion` |
| `/delete` | `/api/v0/chat_session/delete` |
| `/upload_file` | `/api/v0/file/upload_file` |
| `/fetch_files` | `/api/v0/file/fetch_files` |
| `/history_messages` | `/api/v0/chat/history_messages` |
| `/create_session` | `/api/v0/chat_session/create` |

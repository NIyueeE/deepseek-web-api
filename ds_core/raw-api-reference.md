# DeepSeek 后端 API 参考

本文档描述 `ds_core` 内部 `DsClient` 向 DeepSeek 后端发起的原始 HTTP 请求。

## 基本信息

### Base URLs

- `https://chat.deepseek.com/api/v0` — 所有 API 端点
- `https://fe-static.deepseek.com` — WASM 文件下载

### 公共请求头

2026-09 无头浏览器抓包（真实 Web/App 客户端）对齐后的完整头集合。
登录请求同样携带全部 `x-*` 头（另加 `Referer: https://chat.deepseek.com/sign_in`）：

| Header | 说明 |
|--------|------|
| `User-Agent` | 必填，WAF 绕过。默认 `DeepSeek/2.5.0 Android/35`（安卓 App 身份；**桌面 Chrome UA 会被 AWS WAF 以 202 challenge 拦截**，Rust 客户端无法执行 JS challenge） |
| `Authorization: Bearer <token>` | 鉴权请求必填 |
| `X-Ds-Pow-Response: <base64>` | 需要 PoW 的请求必填 |
| `X-Client-Version` | 客户端版本号（默认 `2.5.0`，与真实客户端抓包一致） |
| `X-Client-Platform` | 客户端平台（默认 `android`） |
| `X-Client-Locale` | 客户端语言区域（默认 `zh_CN`） |
| `X-Client-Bundle-Id` | 固定 `com.deepseek.chat` |
| `X-Device-Id` | 设备级 UUID；配置留空时首启生成随机 UUID 并写回配置（每安装一个持久值），亦用于 `check_device` payload |
| `X-Device-Model` | 设备型号，真实客户端发空串 |
| `X-Client-Timezone-Offset` | 时区偏移，UTC+8 = `28800` |
| `X-Hif-Leim` | **风控令牌，仅 completion（SSE）请求需要**，见下节 `0.4` |

登录 payload 的 `os` 字段应与 `X-Client-Platform` 身份一致（`web` / `android`），
由 `client_os` 配置（默认 `android`）。

### 响应信封格式

所有非流式响应使用统一的 `Envelope` 封装：

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": { ... }
  }
}
```

- `code != 0` → 系统级错误（如 40003 无效 Token）
- `biz_code != 0` → 业务级错误
- `biz_data` → 实际数据

### PoW target_path 映射

| 端点 | target_path |
|------|-------------|
| completion | `/api/v0/chat/completion` |
| edit_message | `/api/v0/chat/edit_message` |
| upload_file | `/api/v0/file/upload_file` |

### 错误响应格式

| 情况 | 格式 |
|------|------|
| 字段缺失 | HTTP 422: `{"detail":[{"loc":"body.<field>"}]}` |
| 无效 Token | HTTP 200: `{"code":40003,"msg":"Authorization Failed (invalid token)","data":null}` |
| 业务错误 | HTTP 200: `{"code":0,"data":{"biz_code":<N>,"biz_msg":"<msg>","biz_data":null}}` |
| 登录失败 | HTTP 200: `{"code":0,"data":{"biz_code":2,"biz_msg":"PASSWORD_OR_USER_NAME_IS_WRONG"}}` |

---

## 0. 登录 login

- **URL**: `POST /api/v0/users/login`
- **请求头**: 完整 `x-*` 客户端头集合（见「公共请求头」）+
  `Referer: https://chat.deepseek.com/sign_in`；早期版本只发 `User-Agent`
- **请求体**:

```json
{
  "email": null,
  "mobile": "[phone_number]",
  "password": "<password>",
  "area_code": "+86",
  "device_id": "[SMSdk 生成的真实设备指纹，89 字符 base64]",
  "os": "android"
}
```

- `email` / `mobile`：二选一，另一个传 `null`
- `device_id`：必填字段（省略 → 422）。**值不可伪造**：空串 / 随机 UUID /
  随机 base64 均返回 `RISK_DEVICE_DETECTED`（biz_code=11），必须是浏览器中
  数美 SDK（`SMSdk.getDeviceId()`）生成并真实登录过的指纹；可用无头浏览器按需生成
- `os`：必填（省略 → 422），与 `X-Client-Platform` 身份一致（`web` / `android`，
  由 `client_os` 配置）

- **响应**: 见上方信封；关键字段 `data.biz_data.user.token`（后续所有请求的
  Bearer token）。`user.chat.is_muted = 1` 表示账号被禁言，`mute_until` 为
  解封时间戳——`ds_core` 在登录后立即读取该字段做禁言早检，避免再走一次
  health_check completion
- **错误**: `biz_code=2` / `biz_msg="PASSWORD_OR_USER_NAME_IS_WRONG"`；
  `biz_code=11` / `RISK_DEVICE_DETECTED`（device_id 无效）；
  `biz_code=10` / `USER_IS_BANNED`

---

## 0.1 设备校验 check_device

真实客户端登录成功后立即调用（`ds_core` 在账号初始化时同步该流程）。

- **URL**: `POST /api/v0/users/auth_token/check_device`
- **请求头**: `Authorization` + 完整 `x-*` 客户端头
- **请求体**:

```json
{ "device_id": "<X-Device-Id UUID>", "device_model": "" }
```

- **响应**: `{"biz_data": {"rotate": null}}`
- `rotate` 为 `null` = 不轮换；非 null 时服务端要求轮换令牌（形态未观测到，
  `ds_core` 兼容字符串与 `{"token": "..."}` 两种解析）

---

## 0.2 当前用户 current_user

- **URL**: `GET /api/v0/users/current`
- **请求头**: `Authorization` + 完整 `x-*` 客户端头
- **响应**: 与登录响应的 `user` 对象同构（含 `chat.is_muted` / `mute_until`）

---

## 0.3 会话列表 fetch_page

网页端「历史会话」数据源；`ds_core` 通过 `GET /admin/api/sessions` 暴露，
为 issue #110（会话保存）的基础能力。

- **URL**: `GET /api/v0/chat_session/fetch_page?lte_cursor.pinned=false[&lte_cursor.updated_at=<ts>]`
- **请求头**: `Authorization` + 完整 `x-*` 客户端头
- **响应**（节选）:

```json
{
  "code": 0,
  "data": {
    "biz_code": 0,
    "biz_data": {
      "chat_sessions": [
        {
          "id": "97952d9b-c23a-4d37-8bcf-feb53c88ea83",
          "title": "命令行命令翻译",
          "title_type": "SYSTEM",
          "pinned": false,
          "model_type": "default",
          "updated_at": 1778039195.997
        }
      ],
      "has_more": true
    }
  }
}
```

- 分页游标为 `lte_cursor.updated_at`（上一页末尾会话的 `updated_at`）

---

## 0.4 HIF 风控令牌 hif-leim / hif-dliq（2026-10-07 抓包）

真实客户端启动时轮询两个**无鉴权**的风控令牌端点，并把值缓存进 localStorage
（`hif_leim_cached` / `hif_dliq_cached`），随后在 SSE 请求上以请求头回传。

- **URL**: `GET https://hif-leim.deepseek.com/query`（同构：`https://hif-dliq.deepseek.com/query`）
- **请求头**: 与业务请求相同的客户端拟态头（`x-client-*` / UA / `accept: */*` /
  `referer: https://chat.deepseek.com/`），**不带 `Authorization`**
- **响应**:

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": { "value": "ALytJQRzYqprAAjt5gs6SzkWIc40r551S4+1cQZ/o/Pmc8t6EoPdAu4=.bf4ojFDXrmksVRYz" }
  }
}
```

- **有效期**: 响应头 `x-hif-ttl`（秒，观测值 `600`）；前端按该值定时重新轮询
  （失败时指数退避，1s 起、上限 `hif_max_retry_interval_secs` 默认 600s）
- **使用位置**: `value` 以 `X-Hif-Leim` 头附加到 **`POST /chat/completion`**
  （前端源码中为 `addSSEHeader`）；缺失该头时上游可判定请求并非官方客户端
- `ds_core` 实现见 `ds_core/src/accounts/hif.rs`（TTL 缓存 + 失败退避 +
  账号初始化预热）

### 0.4.1 前端源码里的确切结构（2026-10-07 提取自 `main.*.js`）

```js
// 两个独立轮询器，各自维护缓存键与退避
this.leimPoller = poller(endPoints.leim, t("leim"));
this.dliqPoller = poller(endPoints.dliq, t("dliq"));
endPoints = {
  leim: { url: KV ? "https://hif-leim.deepseek.com/query" : "https://hif-test.deepseek.com/query" },
  dliq: { url: KV ? "https://hif-dliq.deepseek.com/query" : "https://hif-test.deepseek.com/query" },
};
maxBackoffMs = 1000 * getFrozenFeature("hif_max_retry_interval_secs", 600);
// 取值后写入 localStorage：hif_leim_cached / hif_dliq_cached

// 请求头提供者：两个令牌**一起**附加，谁为空就跳过谁
(n = headers.leim || store.leim.get() || "") && (out["x-hif-leim"] = n);
(r = headers.dliq || store.dliq.get() || "") && (out["x-hif-dliq"] = r);
```

要点：

- 生产环境用 `hif-leim` / `hif-dliq` 两个域名；**非生产**（KV 开关关闭）两者都指向
  `hif-test.deepseek.com/query`；
- 两个令牌由同一个请求头提供者一起下发 —— 也就是说“只发 `x-hif-leim`”与真实客户端
  并不完全等价（在 `dliq` 可解析的网络里）。

### 0.4.2 实测：`hif-dliq` 在本网络不可解析（2026-10-07）

- DNS：`hif-leim.deepseek.com` → WAF 地址（60.204.2.5 / huaweicloudwaf）；
  **`hif-dliq.deepseek.com` 无解析结果（NXDOMAIN）**
- 浏览器抓包（同一台机器）：对 `hif-dliq` 共发出 **4 次**请求、收到 **0 次**响应，
  与 DNS 失败一致 —— 即真实客户端在这里也拿不到 dliq 值，`x-hif-dliq` 头被跳过
- 直接用 `curl` 复验：`hif-leim` 返回 200 + `x-hif-ttl: 600` + 73 字符 value；
  `hif-dliq` 直接报 `Could not resolve host`
- 结论：**在本网络下**，“只发 `x-hif-leim`” 与官方客户端行为一致；
  但在 `hif-dliq` 可解析的地区，官方客户端会额外带上 `x-hif-dliq`，此时我们的请求
  仍然少一个头（见 `docs/development.md` 的《剩余差异》）。

---

### 0.5 completion 请求体字段（2026-10-07 从前端 bundle 提取）

真实客户端的 `/chat/completion` 请求体固定包含：

```js
{ chat_session_id, parent_message_id, model_type, prompt, ref_file_ids,
  thinking_enabled, search_enabled, source, action, preempt }
```

- **`source`**：*用户如何发出这条消息* —— `input`（输入框回车）/ `click`（发送按钮）/
  `keyboard` / `paste` / `drag` / `file_picker` / `search` / `landing` / `api` 等。
  代理侧统一填 `input`（真实用户最常态的路径）。
- **`prompt`**：**只包含最新一条用户消息的文本**。历史不在 prompt 里 ——
  它保存在服务端会话中，靠 `parent_message_id` 串联（前端 bundle 中**没有任何**
  `<｜Role｜>` 之类标签字面量）。本代理为了让每轮请求自成一体会把历史内联进 prompt，
  这是 prompt 层面与真实客户端最大的一处不同（详见 `docs/development.md`）。

---

## 1. 创建会话 create_session

- **URL**: `POST /api/v0/chat_session/create`
- **请求头**: `Authorization`, `User-Agent`
- **请求体**: `{}`
- **响应**:

```json
{
  "code": 0,
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "chat_session": {
        "id": "e6795fb3-272f-4782-87cf-6d6140b5bf76",
        "seq_id": 197895830,
        "agent": "chat",
        "model_type": "default",
        "title": null,
        "title_type": "WIP",
        "version": 0,
        "current_message_id": null,
        "pinned": false,
        "inserted_at": 1775732630.005,
        "updated_at": 1775732630.005
      },
      "ttl_seconds": 259200
    }
  }
}
```

- **关键字段**: `data.biz_data.chat_session.id`（后续 completion 用的 `chat_session_id`）
- `ttl_seconds`: 259200（3天），会话有效期

---

## 2. 获取 WASM get_wasm

- **URL**: `GET https://fe-static.deepseek.com/chat/static/sha3_wasm_bg.<hash>.wasm`
- **请求头**: 无需鉴权，无需 User-Agent
- **响应**: 约 26KB，`Content-Type: application/wasm`，标准 WASM 格式（`\x00asm` magic number）
- **注意**: URL 中的 hash 部分可能改变，建议可配置

---

## 3. 创建 PoW Challenge create_pow_challenge

- **URL**: `POST /api/v0/chat/create_pow_challenge`
- **请求头**: `Authorization`, `User-Agent`
- **请求体**:

```json
{
  "target_path": "/api/v0/chat/completion"
}
```

- **响应**:

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "challenge": {
        "algorithm": "DeepSeekHashV1",
        "challenge": "7ffc9d19b6eed96a6fca68f8ffe30ee61035d4959e4180f187bf85b356016a96",
        "salt": "3bde54628ea8413fee87",
        "signature": "ce4678cf7a1290c2a7ac88c4195a5b8497e5fc4b0e8044e804f5a6f3af6fe462",
        "difficulty": 144000,
        "expire_at": 1775380966945,
        "expire_after": 300000,
        "target_path": "/api/v0/chat/completion"
      }
    }
  }
}
```

- 关键字段: `challenge`（哈希输入前缀）、`salt`（拼接用）、`difficulty`（目标阈值）、`expire_at`（过期时间戳 ms）
- `algorithm`: 固定 `"DeepSeekHashV1"`
- `expire_after`: 300000ms = 5 分钟有效期

---

## 4. 对话完成 completion

- **URL**: `POST /api/v0/chat/completion`
- **请求头**: `Authorization`, `User-Agent`, `X-Ds-Pow-Response`（每次请求必须重新计算）,
  **`X-Hif-Leim`（风控令牌，见 `0.4`；缺失即暴露非官方客户端）**
- **请求体**:

```json
{
  "chat_session_id": "<来自 create 端点的 id>",
  "parent_message_id": null,
  "model_type": "default",
  "prompt": "你好",
  "ref_file_ids": ["file-xxx"],
  "thinking_enabled": true,
  "search_enabled": true,
  "action": null,
  "preempt": false
}
```

- `model_type`: `"expert"`（默认）| `"default"` | 等
- `ref_file_ids`: 上传文件后返回的文件 ID 数组，会话级别记忆，后续 `edit_message` 无需重复传入
- `action`: 2026-10 抓包中真实客户端固定发 `null`（会话内首次消息）
- `preempt`: 预占模式（目前网页端未使用），默认 false
- **Response**: `text/event-stream` SSE 流
- 实测请求顺序：`create_pow_challenge` → `completion`；同一会话的后续消息复用
  该 session 并带上一条响应的 `parent_message_id`

### SSE 事件格式

**1. `ready` — 会话就绪**

```
event: ready
data: {"request_message_id":1,"response_message_id":2,"model_type":"expert"}
```

`ready` 后通常紧跟 `event: update_session`，这是正常的会话更新时间，不要误认为流结束。

**2. `update_session` — 会话更新**

```
event: update_session
data: {"updated_at":1775386361.526172}
```

**3. 增量内容 — 操作符格式**

所有增量更新使用统一的数据格式，通过 `"p"`（路径）和 `"o"`（操作符）组合：

| 格式 | 示例 |
|------|------|
| `"p"` 路径 + `"v"` 值 | `{"p":"response/status","v":"FINISHED"}` |
| `"p"` + `"o":"APPEND"` + `"v"` 值 | `{"p":"response/fragments/-1/content","o":"APPEND","v":"，"}` |
| `"p"` + `"o":"SET"` + `"v"` 值 | `{"p":"response/fragments/-1/elapsed_secs","o":"SET","v":0.95}` |
| `"p"` + `"o":"BATCH"` + `"v"` 数组 | `{"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":41},{"p":"quasi_status","v":"FINISHED"}]}` |
| 纯 `"v"` 值 | `{"v":"用户"}`（继续追加到上一 `"p"` 路径）|
| 完整 JSON 对象（初始快照） | `{"v":{"response":{"message_id":2,"fragments":[...]}}}` |

### Delta 解析算法

来自 DeepSeek 前端源码的完整 delta 解析逻辑：

```javascript
class DeltaParser {
    constructor() {
        this.op = "SET";   // 默认操作符
        this.path = "";    // 默认路径
    }

    parse(event) {
        // path/op 跨事件持久化：后续事件可省略 p/o 字段
        let op  = this.op  = event.o ?? this.op;
        let path = this.path = event.p ?? this.path;

        // 非 BATCH：返回单条操作
        if (op !== "BATCH")
            return [{ path, op, value: event.v }];

        // BATCH：分解数组中的每一项
        let subParser = new DeltaParser;
        let results = [];
        for (let item of event.v) {
            let sub = subParser.parse(item);
            for (let s of sub)
                s.path = (path ? path + "/" : "") + s.path;
            results.push(...sub);
        }
        return results;
    }
}
```

**关键规则**：

| 规则 | 说明 |
|------|------|
| `p` 和 `o` 跨事件持久化 | 后续事件可省略 `p`/`o`，沿用上一事件的值 |
| `o` 默认值为 `"SET"` | 无 `o` 字段的事件使用 SET 语义 |
| `APPEND` 对字符串 = `+=` | 纯增量追加 |
| `BATCH` 递归分解 | 子项 `p` 前置父路径 |
| 操作类型只有 3 种 | `SET`（替换）、`APPEND`（追加）、`BATCH`（批量） |

**状态更新引擎逻辑**:

```javascript
switch (op) {
case "SET":
    target[resolvePath(lastPart)] = value;  // 直接赋值
    break;
case "APPEND":
    if (typeof value === "string")
        target[resolvePath(lastPart)] += value;  // 字符串拼接
    else if (Array.isArray(value))
        // 数组合并（push 或 splice 到负索引位置）
    break;
}
```

### SSE 流状态路径

| 路径/字段 | 说明 |
|-----------|------|
| `response/fragments/-1/content` | 最后一个 fragment 的内容 |
| `response/fragments/-1/elapsed_secs` | 思考/搜索耗时（秒），仅 THINK 类型 |
| `response/fragments/-1/status` | fragment 状态 `WIP` → `FINISHED` |
| `response/fragments/-{n}/status` | 负索引标记任意 fragment 完成 |
| `response/conversation_mode` | 会话模式：`"DEFAULT"` 或 `"DEEP_SEARCH"` |
| `response/has_pending_fragment` | 后台有 fragment 处理中时为 true |
| `response/search_status` | `"SEARCHING"` → `"FINISHED"` |
| `response/accumulated_token_usage` | token 用量累计 |
| `response/quasi_status` | BATCH 内结束信号：`"FINISHED"` 或 `"INCOMPLETE"` |
| `response/status` | 主状态 `WIP` → `FINISHED` 或 `INCOMPLETE` |

### Fragment 结构

```typescript
{
  id: number,
  type: "THINK" | "RESPONSE"
      | "TOOL_SEARCH"            // 搜索查询（含 queries + results）
      | "TOOL_OPEN"              // 打开链接（含 result + reference）
      | "TIP",                   // 提示条（含 style + hide_on_wip）
  content: string | null,
  elapsed_secs?: number,         // THINK 类型：思考耗时
  status?: "WIP" | "FINISHED",
  queries?: Array<{ query: string }>,
  results?: Array<{ url: string, title: string, snippet: string, ... }>,
  result?: { url: string, title: string, snippet: string, ... },
  reference?: { id: number, type: "TOOL_SEARCH" },
  style?: "WARNING",
  hide_on_wip?: boolean,
  references?: Array<{ id: number, type: "TOOL_SEARCH" | "TOOL_OPEN" }>,
  stage_id: number
}
```

### 思考内容 vs 实际输出

通过 `fragments[].type` 字段区分：

```
type == "THINK"     → 思考内容（仅 thinking=ON 时出现）
type == "RESPONSE"  → 实际输出内容
```

### 流阶段顺序（thinking=ON, search=ON）

```
 1. SNAPSHOT    → 初始快照，fragments[0].type="THINK"
 2. THINKING    → content APPEND 追加思考内容
 3. THINK END   → elapsed_secs SET
 4. TOOL_SEARCH → APPEND TOOL_SEARCH fragment
 5. SEARCH      → results SET（大量结果）
 6. SEARCH END  → status="FINISHED"
 7. THINK(2)    → APPEND 新 THINK fragment（评估搜索结果）
 8. TOOL_OPEN   → APPEND 多个 TOOL_OPEN fragment
 9. OPEN END    → status="FINISHED"（批量标记）
10. THINK(3)    → APPEND 新 THINK fragment（整理信息）
11. RESPONSE    → APPEND RESPONSE fragment
12. CONTENT     → content APPEND 追加输出
13. REFERENCE   → BATCH 注入引用标记 [reference:N]
14. TIP         → APPEND TIP fragment
15. BATCH       → accumulated_token_usage + quasi_status="FINISHED"
16. DONE        → status="FINISHED"
```

### 流阶段顺序（thinking=OFF, search=OFF）

```
1. SNAPSHOT    → 初始快照，fragments[0].type="RESPONSE"
2. CONTENT     → content APPEND
3. BATCH       → accumulated_token_usage + quasi_status="FINISHED"
4. DONE        → status="FINISHED"
```

### `hint` — 服务端提示/错误

```
event: hint
data: {"type":"error","content":"Content is too long. Please shorten it and try again.","clear_response":true,"finish_reason":"input_exceeds_limit"}
```

- `type`: `"error"` 表示错误提示，其他值可忽略
- `finish_reason`: `"input_exceeds_limit"`（输入超长）、`"rate_limit_reached"`（限流）等
- hint 事件通常出现在 `ready` 后不久，流处理器应在收到 hint 后主动终止

### 流结束序列

**正常完成**:
```
data: {"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":139},{"p":"quasi_status","v":"FINISHED"}]}
data: {"p":"response/status","o":"SET","v":"FINISHED"}

event: update_session
data: {"updated_at":1778639258.866693}

event: title
data: {"content":"Rust所有权概念解释"}

event: close
data: {"click_behavior":"none","auto_resume":false}
```

**手动中断**:
```
data: {"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":39},{"p":"quasi_status","v":"INCOMPLETE"}]}
data: {"p":"response/status","v":"INCOMPLETE"}
```

**最可靠的结束信号是 `response/status` 变为 `FINISHED` 或 `INCOMPLETE`。**

---

## 5. 编辑消息 edit_message

- **URL**: `POST /api/v0/chat/edit_message`
- **请求头**: `Authorization`, `User-Agent`, `X-Ds-Pow-Response`
- **请求体**:

```json
{
  "chat_session_id": "<session_id>",
  "message_id": 1,
  "prompt": "test again",
  "search_enabled": true,
  "thinking_enabled": true
}
```

- **注意**: `model_type` 和 `ref_file_ids` 不在 payload 中——二者在首次 completion 时传入后由 session 级别记忆，后续 edit_message 继承
- `message_id`: 必须已存在（空 session 的 `message_id=1` 会返回 `biz_code=26, "invalid message id"`）
- 编辑后生成新的 `message_id`，需从 SSE `ready` 事件中获取 `response_message_id` 用于后续 `stop_stream`
- **Response**: 同 `completion`（SSE 流）

---

## 6. 停止流 stop_stream

- **URL**: `POST /api/v0/chat/stop_stream`
- **请求头**: `Authorization`, `User-Agent`
- **请求体**:

```json
{
  "chat_session_id": "57bf7fb1-5fde-4d21-a08e-5dfa017216d5",
  "message_id": 2
}
```

- `chat_session_id`: 来自 create 端点的 session ID
- `message_id`: 要取消的响应消息 ID。编辑请求的 `message_id=1` 对应响应 `message_id=2`
- **不需要 PoW header**
- **作用**: 取消正在进行的流式输出。客户端断开连接后调用此端点可让 DeepSeek 侧停止继续生成。

**响应**:
```json
{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}
```

---

## 7. 删除会话 delete_session

- **URL**: `POST /api/v0/chat_session/delete`
- **请求头**: `Authorization`, `User-Agent`
- **请求体**: `{"chat_session_id": "<session_id>"}`
- **响应**:

```json
{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}
```

---

## 8. 更新标题 update_title

- **URL**: `POST /api/v0/chat_session/update_title`
- **请求头**: `Authorization`, `User-Agent`
- **请求体**:

```json
{
  "chat_session_id": "<session_id>",
  "title": "test"
}
```

- **响应**:

```json
{
  "code": 0,
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "chat_session_updated_at": 1775382827.122839,
      "title": "test"
    }
  }
}
```

- **错误码**: `biz_code=5` → `EMPTY_CHAT_SESSION`（空 session 无法设置标题）；`biz_code=1` → `ILLEGAL_CHAT_SESSION_ID`

---

## 9. 上传文件 upload_file

- **URL**: `POST /api/v0/file/upload_file`
- **请求头**: `Authorization`, `User-Agent`, `X-Ds-Pow-Response`（target_path 为 `/api/v0/file/upload_file`）
- **请求体**: `multipart/form-data`，字段名 `file`

```
Content-Disposition: form-data; name="file"; filename="test.txt"
Content-Type: text/plain
```

- **响应**:

```json
{
  "code": 0,
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "id": "file-4387ddbe-efed-4459-83b0-ebb89db61f0f",
      "status": "PENDING",
      "file_name": "test.txt",
      "from_share": false,
      "file_size": 1000,
      "model_kind": "NORMAL",
      "token_usage": null,
      "error_code": null,
      "inserted_at": 1778644590.853,
      "updated_at": 1778644590.853,
      "is_image": false,
      "audit_result": null
    }
  }
}
```

- 关键字段: `data.biz_data.id`（后续 completion 的 `ref_file_ids` 使用）
- 上传后 `status` 为 `PENDING`，需轮询 `fetch_files` 直到 `status=SUCCESS`
- 状态流转: `PENDING` → `PARSING` → `SUCCESS`（或 `FAILED`）

---

## 10. 查询文件状态 fetch_files

- **URL**: `GET /api/v0/file/fetch_files?file_ids=<id>`
- **请求头**: `Authorization`, `User-Agent`
- **响应**:

```json
{
  "code": 0,
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "files": [
        {
          "id": "file-xxx",
          "status": "SUCCESS",
          "file_name": "main.js",
          "from_share": false,
          "file_size": 2836902,
          "model_kind": "NORMAL",
          "token_usage": 619907,
          "error_code": null,
          "inserted_at": 1778644547.106,
          "updated_at": 1778644547.106,
          "is_image": false,
          "audit_result": null
        }
      ]
    }
  }
}
```

- 关键字段: `files[].status` → `SUCCESS` 表示上传完成
- 状态流转: `PENDING` → `PARSING` → `SUCCESS`
- `model_kind`: `"NORMAL"`（文本/PDF）或 `"VISION"`（图片）
- `token_usage`: 文件解析消耗的 token 数（SUCCESS 后才有值）

---

## WASM 故障处理

若 DeepSeek 更新了 WASM 文件导致 PoW 计算失败：

1. `PowSolver` 使用动态导出探测（不硬编码 `__wbindgen_export_0`），自动适配大部分 WASM 变更
2. 如仍失败，更新配置中的 `wasm_url` 指向新的 WASM 文件 URL
3. 参见 `ds_core/src/accounts/pow.rs` 中的动态探测逻辑

## WAF 绕过

- US IP 被 DeepSeek CloudFront WAF 拦截（HTTP 202 / x-amzn-waf-action）
- 配置非 US 代理即可绕过：`[proxy] url = "http://127.0.0.1:7890"`
- `wreq` 使用 BoringSSL，TLS/HTTP2 指纹由 `emulation` 配置选择：
  `okhttp4_12`（默认，原生安卓 App，与默认 UA / `client_platform = android` 自洽）
  或 `chrome136`（桌面 Chrome，需配合 web 身份）
- 2026-10-07 实测：三种身份组合（安卓 App + Chrome136 / 全 Web Chrome / 安卓 App + OkHttp）
  在 `/client/settings`、`/users/login`、`/chat/create_pow_challenge`、`/chat_session/create`
  上均无 202 challenge —— 早期「桌面 Chrome UA 必被拦截」的结论已不适用

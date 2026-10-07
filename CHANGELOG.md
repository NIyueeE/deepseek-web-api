# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- **登录被终止性拒绝时不再反复重试**：账号处于 `Error` 时后台恢复任务每 60 秒重登一次，
  而封禁（`biz_code=10`）/ 禁言（5）/ 设备校验失败（11）/ 凭据错误（2）这类错误重试
  不会改变结果 —— 文档里也写明「禁言后继续重试不会加速解禁，反而可能延长」。
  现在这类错误立即置 `Invalid` 并停止重试（新增 `is_terminal_login_error` 与单测）
- **不再为基础设备身份预热 HIF 令牌**：账号流量一律使用「按账号派生」的设备身份，
  此前启动时还会用一个从不参与业务请求的基础身份多取一份令牌（多一次无谓请求，
  也让一个只出现在取令牌请求里的设备身份出现在上游）；改为在账号初始化时按该账号
  设备身份预热

## [0.6.0] - 2026-10-07

> **验证边界（务必阅读）**：本版把设备身份改为**按账号**派生（X-Device-Id + HIF
> 令牌都按设备分桶），并修掉了 wasmtime 的 critical 安全公告。**「能否规避封号」
> 仍未被证明**：唯一实测账号（9 月已累计多次违规）在最小流量后 19 分钟被判
> `USER_BANNED`，官方提示「由于违规次数过多，你的账户已被临时停用」，属阶梯升级
> 处罚。对照实验（另一账号，只用官方浏览器）仍在观测中。完整时间线与下一步实验
> 设计见 `docs/development.md`。

### Security

- **wasmtime 48.0.2 → 48.0.5**：RUSTSEC-2026-0315 / 0316 / 0325 / **0327（critical,
  9.3）** 等新公告要求 >= 48.0.4（或 >= 49.0.2），CI 的 `cargo audit` 因此失败。
  保持 48.x 分支上升级到 48.0.5（PoW 求解链路已本地验证：WASM 加载/实例化/导出探测
  与升级前一致；合成 challenge 在 48.0.2 与 48.0.5 上表现相同，均为构造值不可解，
  非升级引入）

### Added

- **`ds_core/examples/identity_probe.rs`**：客户端身份变体的 WAF 兼容性探测
  （只打无鉴权的 `/client/settings` 与用一次性假凭据打 `/users/login`，**不产生任何
  账号流量**）。顺带更正 2026-09-20 的过时结论：现在「安卓 App 身份」「全 Web Chrome
  身份」「OkHttp 身份」三种组合都能到达应用层，桌面 Chrome UA 不再被 202 拦截

### Changed

- **设备身份改为按账号派生（X-Device-Id + HIF 令牌都按设备分桶）**：
  `client_device_id` 留空（默认）时，`X-Device-Id` 按该账号的数美 `device_id`
  派生（`ds-free-api:x-device-id:{device_id}`）—— 每个账号一个**稳定且唯一**的设备
  身份，与真实客户端「一个浏览器 profile = 一个数美 device_id + 一个 X-Device-Id」
  一致；HIF 风控令牌也**按设备分别获取与刷新**。此前是实例级共用同一个
  `X-Device-Id` 与同一份令牌，等于告诉上游「这些账号来自同一台设备」。
  显式配置 `client_device_id` 仍可强制共用（不推荐）。同时移除 v0.5.0 引入的
  「首启生成随机 UUID 并写回配置」逻辑（已被按账号派生取代）与
  `ds_core::random_device_uuid`
- 文档（`docs/development.md`）：补上「我们实际发出的请求头 vs 真实客户端」取证 ——
  当前是「安卓 App UA + Chrome/macOS client hints + 文档导航头」的自相矛盾组合，
  并给出两条自洽路线（全 Web 身份 / OkHttp 原生 App 身份）与验证前提

## [0.5.0] - 2026-10-07

> **验证边界（务必阅读）**：本版补齐了真实客户端必带、此前完全缺失的
> `x-hif-leim` 风控令牌，并修掉了两个全局常量指纹。**但「能否规避封号」
> 尚未被证明**：唯一的实测账号（9 月已累计多次违规）在最小流量后 19 分钟
> 被官方判定 `USER_IS_BANNED`，提示语为「由于违规次数过多，你的账户已被
> 临时停用」，属于阶梯升级处罚，无法归因于本次修复失败、也无法证明其有效。
> 完整时间线与下一步实验设计见 `docs/development.md`。

### Added

- **HIF 风控令牌 `x-hif-leim`（2026-10-07 抓包发现，本轮防封号的核心修复）**：
  用真实浏览器对 `chat.deepseek.com` 做**完整对话流程**抓包后发现，真实客户端会
  轮询 `GET https://hif-leim.deepseek.com/query`（**无鉴权**，返回
  `data.biz_data.value`，有效期取响应头 `x-hif-ttl`，默认 600s），并在
  **completion（SSE）请求**上以 `x-hif-leim` 头回传（前端源码中该头由
  `addSSEHeader` 注入，且仅注入 SSE 请求）。
  - `ds_core` 新增 `accounts/hif.rs`：按 TTL 缓存 + 到期前 30s 刷新 +
    失败退避 60s，账号初始化时预热（真实客户端在应用启动时即开始轮询）
  - 取令牌失败不阻断业务请求（与真实客户端轮询失败时的行为一致）
  - 新配置项 `hif_enabled`（默认 `true`，管理面板设置页可关，**仅用于对照实验**）
  - 这一项此前完全缺失：请求 `/chat/completion` 却不带该头，上游可直接判定
    请求并非来自官方客户端 —— 与「每账号仅 2 次请求也被禁言」的实测现象吻合
- **无头浏览器抓包对齐（2026-09-20 实测）**：用 Playwright 对
  `chat.deepseek.com` 真实登录流程抓包，逐项修正 `ds_core` 的客户端拟态：
  - 登录请求补齐全部 `x-*` 头：`X-Client-Bundle-Id` / `X-Device-Id` /
    `X-Device-Model` / `X-Client-Timezone-Offset` / `X-Client-Version` /
    `X-Client-Platform` / `X-Client-Locale`（此前登录仅发 `User-Agent`）
  - 新增 3 个原始端点：`POST /users/auth_token/check_device`（令牌轮换，
    兼容字符串与 `{"token":…}` 两种 rotate 形态）、
    `GET /users/current`、`GET /chat_session/fetch_page`（分页，游标
    `lte_cursor.updated_at`）
  - `GET /admin/api/sessions`：账号会话列表（分页，响应带 `has_more` /
    `next_cursor`），issue #110 的基础能力
  - 新配置项（`[ds_core]`）：`client_os` / `client_bundle_id` /
    `client_device_id` / `client_device_model` / `client_timezone_offset`，
    管理面板设置页可配
- **禁言早检**：账号初始化时直接读取登录响应 `user.chat.is_muted` /
  `mute_until`，命中即止——不再创建 session、不再发送 health_check
  completion（禁言账号 health_check 必然失败，省掉一次完整请求）
- **单账号每小时请求配额** `hourly_request_quota`（默认 60，0 = 不限制）
  - 账号维度的一小时滑动窗口计数（`SlidingWindowRateLimiter`，见 PR #114）
  - 用尽的账号在本窗口内不再被分配，由池中其他账号承接；
    **全部账号都用尽时返回 429**，而不是继续硬打上游
  - 账号状态接口 / 管理面板显示「本小时已用」与「配额已用尽」
  - 设置页可调；`config.example.toml` 与 docker 示例同步
- **共用 `device_id` 启动告警**：检测到多个账号共用同一设备指纹时，
  在日志中列出涉及账号并给出修复建议（不阻止启动）

### Changed

- **设备 UUID 改为「每安装一个持久 UUID」**：`client_device_id` 留空时，
  首次启动会生成随机 UUID 并写回配置文件（此前是按 `api_base` 确定性派生，
  意味着**所有部署共用同一个 X-Device-Id**，上游可据此关联大量账号）。
  文档与两份 `config.example.toml` 同步
- **health_check 提示词不再固定**：此前固定发「只回复\`Hello, world!\`」——
  这是所有部署共用的常量字符串，按 prompt 聚类即可关联大量账号；
  现改为中性短提示词池中随机取一条
- **登录 payload 字段对齐真实客户端**：`email` / `mobile` / `area_code`
  固定发送（空值发空串，而非省略或 null）
- **`device_id` 策略更正**：文档从「设备级、可复用于多个账号」改为
  **「每个账号使用独立 device_id」**。该指纹是设备级的，上游用它做关联与画像
- **客户端拟态默认值升级**：`user_agent` 默认 `DeepSeek/2.1.1 Android/35` →
  `DeepSeek/2.5.0 Android/35`，`client_version` 默认 `2.0.0` → `2.5.0`
  （与真实客户端抓包一致，实测可通过 WAF 并正常登录）；登录 payload 的
  `os` 由硬编码 `"web"` 改为配置项 `client_os`（默认 `android`，与
  `X-Client-Platform` 身份保持一致）

### Fixed

- **UTF-8 切片 panic（多语言输入）**：错误消息 / trace 日志按字节截断预览文本，
  中文 / emoji 输出下会切到 UTF-8 续字节而 panic。改为 `floor_char_boundary` /
  按字符截断：
  - `src/openai_adapter/response.rs` —— 工具修复失败的错误消息预览
  - `src/openai_adapter/response/tool_parser.rs` —— 两处解析失败 trace 预览
  - `ds_core/src/chat/request.rs` —— 两处测试断言消息
- **非 ASCII API Key / 账号 ID 脱敏 panic**：`api_keys` 未约束为 ASCII，
  `&key[..8]` 之类的字节切片在含中文 / emoji 的 key 上会 panic。
  新增 `server::mask_prefix` 统一按字符截断（`handlers.rs` / `stats.rs` / `config.rs` 共 6 处）
- **e2e 测试框架读取旧配置段名**：`py-e2e-tests/config.py` 仍读 `[deepseek]`
  （v0.2.x 已更名为 `[ds_core]`），导致永远按 `default/expert/vision` 三个模型测试
  （后两者上游已下线，产生大量假失败），且账号数恒为 0、并发恒为 1。
  改为读取 `[ds_core]`，默认值与 `src/config.rs` 的 `default_*` 对齐

### Docs

- 统一 `device_id` 策略描述：`README.md` / `README.en.md` / `docs/development.md`
  此前仍写「设备级、可在多账号间复用」，与本次更正后的「每账号独立」结论矛盾；
  现均改为每账号独立，并注明伪造值会被 `RISK_DEVICE_DETECTED`（biz_code 11）拒绝

### 为什么改这个，而不是改 prompt 注入格式

仓库内 `stats.json` 提供了两次封禁的对照数据：

| | 维护者压测（v0.2.9 期间） | 本次会话（v0.3.0） |
|---|---|---|
| 时间 | 04:21–07:30 UTC | 11:14–12:11 UTC |
| 请求数 | **217** | **217** |
| prompt 格式 | **旧格式（`<think>` 注入）** | **标准 ChatML** |
| `device_id` | 同一个 | **同一个** |
| 结果 | 跑完**之后**账号1 被禁 | 跑完**之后**账号2 被禁 |

**格式变了，结果没变。** 且 v0.2.9 的 CHANGELOG 已记录过同样的结论
（「提示词改动**可能**有帮助，但**封禁并未消除**」）。因此「再找一个更合适的注入格式」
属于已经被证伪的方向；两次事件的共同点是**请求量**与**共用指纹**。

### 实测补充：`device_id` 不能伪造

同一未封禁账号的 A/B（三种 device_id）：

| device_id | 结果 |
|---|---|
| 真实浏览器注册的指纹 | ✅ 通过设备校验 |
| 伪造的 base64（88 字符） | ❌ `RISK_DEVICE_DETECTED` (11) |
| 伪造的普通字符串 | ❌ `RISK_DEVICE_DETECTED` (11) |

因此「每账号独立 device_id」需要**为每个账号各自注册一次设备**，
不能靠生成随机值代替 —— 这是一项真实成本，已在配置示例中写明。

> 排查方法提示：不要用**已封禁**账号验证此事。封禁检查可能先于设备校验，
> 返回 `USER_IS_BANNED` 会让人误判为「伪造值也通过了」。

### 2026-09-17 实测：配额内的单账号压测**仍被禁言**

三个账号解禁后**逐号单独**验证（每号独立启动，跑一轮 basic + repair，约 27 次上游请求，
远低于 60 次/小时配额）：

| 账号 | 初始化 | 全量 e2e | 复查结果 |
|---|---|---|---|
| `l3366599051@163.com` | 04:08:53 ✅ | 04:09–04:12（basic 13/14 + repair 10/10） | **04:21 已禁言**，`mute_until` ≈ 09-26 04:18 |
| `1460183479@qq.com` | 04:12:54 ✅ | 04:14–04:17（basic 13/14 + repair 10/10） | **04:21 已禁言**，`mute_until` ≈ 09-26 04:18 |
| `n1yu3@proton.me` | 04:17:21 ✅ | 04:19–04:21（basic 14/14 + repair 10/10） | 04:29 复查正常 → **06:58 已禁言**，`mute_until` ≈ 09-26 04:34 |

> 三次 basic 中仅有的失败均为上游 `code=7, rate limit reached` 的文件上传限流
> （重试 3 次后仍失败），与 prompt 注入无关；default 模型的对话 / 工具 / 流式 / 推理场景全部通过。

结论与局限：

- **三个账号最终全部被禁言**：账号 1、2 在跑完数分钟内被禁言，账号 3 在 04:29 复查时仍正常、
  但 06:58 复查已禁言（`mute_until` ≈ 09-26 04:34，判定时间比账号 1、2 晚约 15 分钟）；
- 这再次证明**禁言是延迟判定的**：短时间内「仍然正常」不能作为安全证据；
- 本次实验三个账号**共用同一个真实浏览器 `device_id`**（当前环境直连被 AWS WAF 拦截，
  无法为每个账号各生成一个真实指纹），且跑的是同一份注入负载 —— 因此**无法区分**
  「共用指纹」与「当前注入 / 请求行为」各自的贡献，两者都不能排除；
- 下一步的正确实验：为每个账号各注册一个真实 `device_id`（消除指纹混杂）后重跑本表流程。
  在这个混杂因素被消除之前，「配额 + 标准 ChatML 注入」都不能称为已验证的安全策略。

> 实际使用建议：本代理**无法保证账号不被风控**；不要用长期账号压测，
> 出现 `biz_code=5` 后立即停用等待解禁。

## [0.4.0] - 2026-09-13

幂等性与架构审查：修复 session 泄漏、统计竞态、CORS 失败安全、模型 ID 一致性，
并把此前形同虚设的搜索开关落实为可配置项。经代码验证后**放弃** session 复用方案（见下）。

### Added

- **`default_search_enabled` 配置项**：请求未携带 `web_search_options` 时是否默认开启搜索模式。
  默认 `true` 保持历史行为；设为 `false` 则严格遵循 OpenAI 语义（未传即关闭），
  可避免注入 DeepSeek 的搜索系统提示词。管理面板「设置」页新增开关，三语言同步
- **`SessionGuard`（RAII）**：临时 session 的删除责任由守卫承担，
  正常路径 `disarm()` 移交 `SessionHandle`；`ds_core` 日志会打印
  `session guard deleting orphan session` 便于观测
- `resolver::resolve()` / `models` 的单元测试（搜索开关、大小写、别名、去重、往返一致性）

### Fixed

- **幂等性 / 资源泄漏：session 未删除**
  分块路径用 `wait_ready_and_update(..).await?` 与 `wait_close(..).await?` 直接传播错误，
  跳过了手写的 `delete_session`，导致孤儿会话在上游累积。
  改为 RAII 守卫后，所有提前返回路径都被覆盖；同时删除了 6 处重复的手写清理。
  （注：实测 39 次 session 创建中守卫触发 20 次，均为**旧代码已手动处理**的路径 ——
  该改动把「依赖每处手写记得清理」变成「结构上不可能遗漏」，并为已识别的 `?` 路径兜底）
- **幂等性 / 统计重复计数：Anthropic `output_tokens` 用 `fetch_add`**
  `message_delta.usage.output_tokens` 按 Anthropic 规范是**累计值**
  （MessageDeltaUsage 原文 "cumulative number of output tokens"），
  累加会在事件重复时重复计数；改为 `store` 整体覆盖
- **幂等性 / 持久化乱序覆盖：`Stats::persist_now()` 并发写盘**
  原实现每次 spawn 一个异步写盘任务，可能**乱序完成**导致旧快照覆盖新快照
  （stats.json 数值回退），且 `write_json_file` 使用固定的 `stats.json.tmp`
  会被并发写互相踩踏。现在用 `tokio::sync::Mutex` 串行化，
  并把快照读取移到**持锁之后**，保证最后一次写入必然最新
- **中间件 / CORS 失败安全**：配置了白名单但无一项能解析成合法 Origin 时，
  旧实现静默回退到 `permissive`（实际完全放开，与用户意图相反）；
  现在回退到**拒绝所有跨域来源**并打警告，同时在警告信息里提示缺少 scheme 这类常见错误
- **中间件 / CORS 缺头**：`allow_headers` 未包含 `x-api-key` 与 `anthropic-version`，
  浏览器端 Anthropic 客户端会被 preflight 拒绝
- **模型层 / 架构：`list()` 与 `get()` 的 ID 生成逻辑重复**
  两处各写一遍，已产生真实缺陷：别名查询返回**小写化后**的 ID，
  而列表返回别名的**原始大小写**，同一模型有两个 ID。
  现在统一由 `model_ids()` 生成有序 ID 集合，两者共用，并新增往返一致性回归测试。
  同时处理了空白别名、`deepseek-` 前缀重复、别名数组短于 `model_types` 等边界
- **模型层 / 死开关**：`resolver::resolve()` 里
  `web_search_options.map(|_| true).unwrap_or(true)` 恒为 `true`，
  `web_search_options` 传与不传毫无区别；文档却声称「省略即关闭」。
  这是自初始提交起就存在的死代码，现已落实为真实开关

### Fixed（文档）

- **`AGENTS.md` 的账号初始化流程描述有误**（据代码核实）：
  - 第 4 步写作 `update_title`，但该函数在 `ds_core/src/accounts/client.rs` 中
    **没有任何调用点** —— 实际第 4 步是 `delete_session`（健康检查后清理临时 session，
    失败路径同样会清理）
  - "每个账号失败后重试 3 次，仍失败则标记 `InitFailed`" 不成立：
    `AccountPool::init()` 内**没有重试**，失败即标记 `Invalid`（且**不存在 `InitFailed` 状态**，
    实际状态为 `Idle`/`Busy`/`Error`/`Invalid`）。重试位于后台恢复任务
    （`start_recovery_task`，每 60s 重登 `Error` 账号，连续失败达 `MAX_ERROR_COUNT`=3 次转 `Invalid`）
  - 同时把「`device_id` 可选」更正为**必填**：实测不带 `device_id` 登录直接被风控拒绝
    （`RISK_DEVICE_DETECTED`，biz_code 11）

### Changed

- **管理面板配置接口**：`GET /admin/api/config` 的 `ds_core` 新增
  `default_search_enabled`（修复「后端有字段、前端读不到」的同类契约缺口；
  契约测试同步扩展）
- `config.example.toml` 与 `docker/config.example.toml` 补充搜索模式说明
- `AGENTS.md`：修正 Web search 能力开关描述；补充配置示例同步要求
- `docs/development.md`：新增「Session 生命周期与每请求新建 session 的取舍」章节

### 决策记录：不实现 session 复用

曾计划通过复用 session 减少「每请求 create/delete」的风控指纹，**经代码验证后放弃**：

分块路径用 `parent_message_id` 串联 chunk，证明**上游 session 会累积对话上下文**；
而正常路径发送 `parent_message_id: None` + 已含完整对话的 prompt。两者叠加意味着
跨请求复用 session 会让模型同时看到「上一轮对话」+「本轮完整对话」。

后果不止是回答质量下降 —— 账号池为多请求共享，**用户 A 的对话会残留在 session 中
并被用户 B 读取**，构成跨用户数据泄漏。上游未提供会话清空能力，因此无法在复用前提下
消除该风险。详见 `docs/development.md`。

### 测试结果

- `cargo test --workspace --all-targets`：**209 passed / 0 failed**（v0.3.0 为 198）
- `cargo clippy --all-targets -- -D warnings`、`cargo fmt --all --check`：通过
- `bun run typecheck` / `lint` / `check:locales` / `build`：通过
- 实机验证（真实账号）：
  - 模型 ID：`MyAlias` / `myalias` / `MYALIAS` 均返回列表中的规范拼写 `MyAlias`；
    列表无重复 ID
  - `default_search_enabled=false` 时管理接口正确回读，推理仍返回 200
  - Responses API 7/7、basic 套件 12/14（2 项为上游 `code=7 rate limit reached`
    文件上传限流，属已知限制；同轮的 Anthropic 文件/图片上传均成功）

> **⚠️ 风控更正（重要）**：先前记录的「累计 216 次请求后账号仍未被禁言」是
> **时间受限的观察，不能作为结论**。该账号在随后 21 分钟内（期间**几乎没有新流量**，
> 仅一次健康检查）被禁言至 09-16 12:16 UTC，说明**禁言是延迟判定的**。
>
> 由此**推翻**了「规范 ChatML 注入即可规避风控」这一此前被写在文档里的推断 ——
> 本次在规范 ChatML 下仍被禁言，说明 prompt 格式不是唯一或决定性因素。
> 完整时间线、各假设的证据强度与实务建议见 `docs/development.md`。
>
> 本代理**无法保证账号不被风控**，只能降低触发概率。

## [0.3.0] - 2026-09-13

新增 OpenAI Responses API 端点，并按上游规范逐条核对了 Chat Completions 与
Anthropic Messages 的实现；同时补齐 CI/CD 契约、前后端联调契约与测试基线。

### Added

- **`POST /v1/responses`（OpenAI Responses API）**：新建 `src/responses_adapter/`
  （`types.rs` / `request.rs` / `response.rs` / `store.rs`），纯协议翻译层，不直接访问 `ds_core`
  - `input` 支持字符串与输入项数组：`message`（含省略 `type` 的形态）、
    `function_call`、`function_call_output`、`item_reference`
  - `instructions` 支持字符串与 `input_text` 数组两种形态；`developer` 角色降级为 `system`
  - 工具支持扁平 Responses 结构与嵌套 Chat Completions 结构；`web_search_preview` 触发搜索模式
  - `text.format`（`json_object` / `json_schema`）→ `response_format`
  - 非流式返回完整 Response 对象（`output` / `output_text` / `usage` 等字段名与规范一致）
  - 流式逐事件输出：`response.created` → `response.in_progress` →
    `response.output_item.added` → `response.content_part.added` /
    `response.reasoning_summary_part.added` → `response.output_text.delta` /
    `response.function_call_arguments.delta` / `response.reasoning_summary_text.delta` →
    `*.done` → `response.output_item.done` → `response.completed`（或
    `response.incomplete` / `response.failed`）→ `data: [DONE]`；
    每个事件都带与 `event:` 同名的 `type` 与单调递增的 `sequence_number`
  - `previous_response_id`：进程内、有界、带 TTL 的上下文缓存；容量/TTL 由
    `responses_store_capacity`（默认 256）与 `responses_store_ttl_secs`（默认 3600）控制；
    引用未知/过期 ID 返回 400（而非 500）
- **`docs/responses-api.md`**：Responses API 字段表、事件序列、`previous_response_id`
  取舍说明与未实现清单
- **`docs/compat-audit.md`**：对照 `openai-openapi` / `openai-python` /
  `anthropic-sdk-typescript` / docs.anthropic.com 的兼容性审计（已修复项、确认合规项、
  有意未实现项、客户端互操作矩阵）
- **`deny.toml`**：cargo-deny 许可证白名单、禁用 crate（`openssl-sys` / `native-tls`）与
  registry 来源校验；`graph.targets` 限定为实际发布的 5 个目标（避免仅 Windows 生效的
  传递依赖干扰许可证判断），并 clarify `wreq-util` 的弃用 SPDX 标识
- **许可证声明修正**：`ds_core` 原本未声明 `license`（cargo-deny 报 `unlicensed`）；
  `Cargo.toml` 的 path 依赖未带 `version`（触发 `wildcards = "deny"`）；
  两个 crate 的 `GPL-3.0` 是被弃用的 SPDX 标识，且仓库无 "or later" 授权说明，
  精确化为 `GPL-3.0-only`
- **`scripts/check-lint-exemptions.sh`**：把 AGENTS.md 的「除 `client.rs` 外禁止 `#[allow]`」
  与「日志必须为英文」两条约定变成 CI 可执行检查
- **`web/scripts/check-locales.mjs`**：三个 locale 文件键集一致性检查（`bun run check:locales`）
- **`scripts/check-config-drift.sh`**：校验 `docker/config.example.toml` 与根目录
  `config.example.toml` 的生效配置一致（唯一允许差异为 `host`）
- **`py-e2e-tests/test_responses.py`**（`just e2e-responses`）：Responses API 端到端测试
  —— 非流式/流式、工具调用、`previous_response_id` 多轮、错误信封
- **HTTP 层集成测试**（`src/server.rs`）：用 `tower::ServiceExt::oneshot` 驱动 axum Router，
  覆盖鉴权中间件、两种错误信封、CORS 白名单行为（新增 `tower` dev-dependency）
- **前后端契约测试**（`src/server/admin.rs`）：直接解析 `web/src/lib/api.ts` 的 TS interface，
  断言 `GET /admin/api/config` 的 JSON 包含前端声明的每个字段
- **Responses API 单元测试**：45 个用例覆盖请求映射、事件顺序、`sequence_number` 单调性、
  usage 延迟发出、`length → incomplete`、上游错误 → `response.failed`、缓存容量/TTL/克隆共享

### Fixed

- **Anthropic 端点无法用 Anthropic SDK / Claude Code 鉴权（BLOCKER）**：`extract_bearer_token()`
  只读 `Authorization: Bearer`，而 Anthropic 官方 SDK 默认发 **`x-api-key`**。
  新增 `extract_api_token()`：优先 Bearer，回退 `x-api-key`
- **Anthropic 错误信封结构错误**：原实现把 error kind 放在顶层 `type`，正确形态是
  `{"type":"error","error":{"type":...,"message":...}}`。Anthropic SDK 依赖
  `error.error.type` 分派错误类，结构不符会退化成无法识别的 `APIError`。
  `/anthropic/*` 的 401/404 现在也返回 Anthropic 信封（`/v1/*` 仍返回 OpenAI 信封）
- **Chat Completions 的 `obfuscation` 字段位置错误**：规范中它是 **chunk 顶层字段**
  （`CreateChatCompletionStreamResponse.obfuscation`），原实现放在 `choices[].delta.obfuscation`，
  严格按 schema 反序列化的客户端会因未知字段报错
- **未请求 usage 时提前下发 usage**：`stream_options.include_usage` 为 false 时，
  原实现仍在 role chunk 上附带 `usage`；规范要求此时整个流不含 usage
- **OpenAI 错误响应缺少 `param` 字段**：`Error` schema 要求
  `type` / `message` / `param` / `code` 四字段齐备
- **Anthropic `stop_reason` 可能输出非法枚举值**：`length` / `content_filter` 被原样透传，
  现在映射为 `max_tokens` / `refusal`，未知取值告警并退化为 `end_turn`；
  非流式响应在无 `finish_reason` 时也保证 `stop_reason` 非空
- **`/v1/models` 缺少裸 model_type 名**：`model_registry()` 接受 `default`（issue #99），
  但模型列表只输出 `deepseek-default`，客户端拉取列表后仍找不到可用模型
- **非流式 `finish_reason` 可能为 `null`**：上游 EOF 未给 finish_reason 时退化为 `stop`
- **RepairStream 心跳伪造空 tool_call**：工具修复等待期间的空 `tool_calls`
  会被客户端按 index 累积成新的空工具调用（issue #87 的同类症状）
- **请求解析错误提示重复前缀**：输出 `bad request: bad request: ...`，现在为
  `bad request: invalid JSON body: ...`
- **管理面板配置页无法编辑 `input_character_limits`**：该数组参与
  `Config::validate()` 的长度校验，但前端既不展示也不同步；增删模型类型时
  `max_input_tokens` / `max_output_tokens` / `input_character_limits` / `model_aliases`
  会与 `model_types` 长度失配，导致保存被后端拒绝
- **`GET /admin/api/config` 缺少 `responses_store_capacity` / `responses_store_ttl_secs`**：
  新配置项未出现在管理接口中，前端设置页无法读取或保存
- **`normalizeConfig()` 默认值与后端不一致**：前端兜底默认值（`2.0.4` / `2.0.4` 客户端版本）
  与 `src/config.rs` 的 `default_*` 不同，会把过期版本号写回服务端；
  且 `input_character_limits` 为空数组时未按 `model_types` 长度补齐
- **Service Worker 缓存策略导致发版后页面不更新**：原实现对所有静态资源（含 `index.html`）
  使用 stale-while-revalidate，用户会持续拿到旧 bundle；导航请求改为 network-first，
  离线时回退缓存

### Changed

- **`.github/workflows/release.yml` 新增两级发布门禁**：
  `verify`（秒级）校验 tag 与 `Cargo.toml` / `ds_core/Cargo.toml` / `web/package.json`
  一致且 `CHANGELOG.md` 存在对应条目；`test` 先下载 `web-dist` 再跑完整测试套件
  （必须在编译前拿到前端产物，否则 `rust_embed` 会嵌入空资源）。
  平台构建全部改为 `cargo build --release --locked`；Docker 构建开启
  `provenance` 与 `sbom`；`permissions` 收敛为按需授予
- **新增 `build.rs`：把前端产物缺失从「静默失败」变为「显式失败」**。
  `rust_embed` 的 `#[folder = "web/dist/"]` 在目录缺失时不会报错，只生成空的嵌入资源，
  于是 `cargo build --release` 能成功但发布出的二进制完全没有管理面板。
  现在 release 构建直接失败并给出修复指引，debug 构建（`cargo check` / `cargo test`）
  仅打印 `cargo:warning`，不阻断尚未构建前端的本地开发
- **`.github/workflows/ci.yml` 重构**：新增 `changes` 路径门禁（文档-only 改动跳过 Rust 作业）与
  `concurrency` 取消策略；`check` / `test` / `security` 三个独立作业；
  工具安装改用 `taiki-e/install-action`；`cargo check/clippy/fmt` 全部覆盖 `--all-targets`；
  新增 cargo-deny 许可证/来源校验、lint 豁免门禁与 i18n 键集门禁
- **日志消息统一为英文**（`docs/logging-spec.md` 的既有约定此前未落地）：52 条中文日志
  改为英文，并由 `scripts/check-lint-exemptions.sh` 持续检查
- `docs/logging-spec.md` 补充 `responses_adapter` / `config` / `store` / `stats` 的 target 映射
- `config.example.toml` 补充 Responses API 上下文缓存的配置说明
- `README.md` / `README.en.md` 更新为「三协议支持」，端点表加入 `/v1/responses`
- `AGENTS.md` 补充 Responses API 层、新 CI 流程、前后端配置契约与新增检查脚本

### 测试结果

- `cargo test --workspace --all-targets`：**198 passed / 0 failed**（v0.2.11 为 132）
- `cargo clippy --all-targets -- -D warnings`、`cargo fmt --all --check`：通过
- `bun run typecheck` / `bun run lint` / `bun run check:locales` / `bun run build`：通过
- 实机验证（真实账号，账号因上游 `user is muted` 无法完成推理）：
  - `/v1/responses` 参数校验全部返回 400 + OpenAI 错误信封（含 `param` 字段）；
    账号池不可用时返回 429 + `retry-after: 30`
  - `/anthropic/v1/messages` 携带 `x-api-key` 可通过鉴权；缺凭据时返回
    `{"type":"error","error":{"type":"authentication_error",...}}`
  - `/anthropic/v1/models/nope` 返回 404 + Anthropic `not_found_error` 信封
  - `/v1/models` 同时列出 `deepseek-default` 与 `default`；`/v1/models/default` 可查询
  - 管理面板配置的读取 / 修改 / 回写全链路验证（`responses_store_capacity` 由 256 改为 64 并落盘）

## [0.2.11] - 2026-09-13

依据抓取到的上游实际配置做精简，并修复 issue #99 / #87 / #76。

### 上游事实（`/api/v0/client/settings?did=<device_id>`）

`model_configs` 是权威来源，网页端已无 expert / vision 切换入口：

| model_type | 名称 | enabled | switchable | input_character_limit |
|------------|------|---------|------------|-----------------------|
| `default` | 快速模式 | ✅ true | ✅ true | 2621440 |
| `expert` | 专家模式 | ❌ **false** | ❌ false | 2621440 |
| `vision` | 识图模式 | ❌ **false** | ❌ false | 2621440 |

另核对 `pow_header_paths` / `authed_pow_functions`（`["search","deep_think","completion","file"]`）
确认现有 PoW 目标路径无需改动。

### Changed

- **默认只暴露 `default`**：`model_types` 默认值由 `["default","expert","vision"]` 收窄为 `["default"]`；
  `input_character_limits` 由 `[2621440,163840,2621440]` 改为 `[2621440]`
  （上游对全部 model_type 都返回 2621440，expert 的 163840 已过期）。
  expert 的 chunked 回退路径**保留**，显式配置 `model_types` 时仍可用

### Fixed

- **issue #99 模型名 `default` 无法识别**：`model_registry()` 原本只注册 `deepseek-<type>` 与别名，
  而 Claude Code / Codex 常把 `model` 设为 `default`。现在额外注册裸 model_type 名（大小写不敏感）
- **issue #87 反复工具调用**：`tool_parser` 的保活心跳每秒发送
  `tool_calls: [{id:"", name:"", arguments:""}]`，客户端按 index/id 累积时会得到一串空工具调用。
  改为发送**空 delta** 心跳
- **issue #76 思考内容出现 `tool_calls...`**：Anthropic 层把保活转成 thinking 增量的字面文本，
  污染思考内容。改为发送 Anthropic 协议规定的 `ping` 事件，且不再打断当前文本块
- **issue #93（第二个入口）`tool_calls` 场景 `completion_tokens` 为 0**：`tool_parser` 有两个
  `ToolParseState::Done` 分支，工具调用后模型继续输出文字时会命中第一个分支并**提前结束流**，
  丢掉随后带 `usage` 的收尾 chunk。现在第一个分支只丢弃幻觉内容、不提前结束流
- **Docker 配置**：`docker/config.example.toml` 同步上游模型状态说明
- **AGENTS.md**：修正过时的 `<think>` 注入描述、**并不存在的 `oversized_prompt` 配置节**、
  以及错误的「api_keys 为空时无鉴权」说明（实测始终 401）

### Added

- 5 项回归测试：`bare_model_type_name_is_accepted`、`keepalive_emits_empty_delta`、
  `keepalive_emits_ping_not_fake_thinking`、改造后的 `keepalive_during_text`、
  `stream_tool_calls_preserves_usage`

### 测试结果

- `cargo test --workspace`：**132 passed / 0 failed**
- 模型列表实测：默认配置仅返回 `deepseek-default`；显式配置三模型时仍返回全部三个（升级安全）
- 裸名实测：`model: "default"` 通过解析；`model: "nonexistent"` 正确返回「不支持的模型」

> **风控实测记录（重要）**：本账号在完整跑完 basic + repair 全量 e2e（70 请求）期间**未被禁言**，
> 但随后仍被上游禁言（`biz_code=5`，`mute_until` 约 3 天后）。
> 因此提示词改动**可能**有帮助，但**封禁并未消除**，风控仍是当前最大风险，请勿据此认为已解决。


## [0.2.10] - 2026-09-13

提示词回归标准 ChatML，去掉容易被上游风控命中的注入特征；同时修复多轮历史缺少生成锚点、
以及 `tool_calls` 场景 `completion_tokens` 恒为 0。

### Changed

- **提示词回归标准 ChatML**：原实现把工具定义与调用规则**重复注入两遍** ——
  `<｜System｜>` 段末尾一份完整 reminder，末尾再追加
  `<｜Assistant｜><think>嗯，我刚刚被系统提醒需要遵循以下内容:...`（不闭合的 `<think>`
  + 角色扮演式元指令）。现在工具定义 / 格式规范 / 调用指令 / `response_format` 约束
  统一作为**普通 System 内容注入一次**，彻底移除未闭合 `<think>` 与元指令措辞

  A/B 实测（同一账号、同一工具请求，对照 prompt 仅差包装方式）：

  | 方案 | 工具调用结果 | prompt 长度 | 上游累计 token |
  |------|--------------|-------------|----------------|
  | 旧：`<think>` 注入 + 重复两遍 | `get_weather {"city":"北京"}` ✅ | 2652 字符 | 1414 |
  | 新：标准 ChatML 注入一次 | `get_weather {"city":"北京"}` ✅ | ~1400 字符 | 802 |

  模型遵循度一致，token 成本降低约 43%；同时消除了「未闭合标签 + 元指令 + 重复块」
  这三个可疑特征（与 issue #97/#101/#102 的封禁反馈吻合）

### Fixed

- **多轮历史缺少生成锚点**（既有 bug）：`prompt.rs` 末尾判断的是「是否**出现过**
  `<｜Assistant｜>`」而非「最后一段是否是」。多轮历史本身就含 assistant 轮次，
  因此不会补生成锚点，`split_history_prompt` 找不到拆分点，整段历史被当作
  inline prompt 直接发送。改为「最后一段不是 `<｜Assistant｜>` 才追加」
- **`tool_calls` 场景 `completion_tokens` 恒为 0**：`tool_parser` 有两个
  `ToolParseState::Done` 分支。工具调用之后模型继续输出文字时会命中第一个分支，
  该分支立刻发出结束 chunk 并置位 `finish_emitted`，导致随后携带
  `finish_reason` + `usage` 的收尾 chunk 被丢弃（实测上游 `usage=811` 但对外报 0）。
  现在第一个分支只继续丢弃幻觉内容、不提前结束流，流结束分支在未发出结束 chunk 时补发

### Added

- `examples/prompt_probe.rs` — 绕过适配层直接向 ds_core 发送任意 prompt，
  用于 A/B 对比不同提示词包装方式。用法：

  ```bash
  PROBE_VARIANTS=/tmp/variants.json PROBE_MODEL_TYPE=default \
    cargo run --example prompt_probe -- -c py-e2e-tests/config.toml
  ```

- 3 项回归测试（均已在未修复代码上确认失败）：
  `prompt_ends_with_assistant_anchor_for_multiturn_history`、
  `tools_injected_into_system_message_once`、`stream_tool_calls_preserves_usage`

### 测试结果

- `cargo test --workspace`：**130 passed / 0 failed**
- `cargo clippy -- -D warnings`、`cargo fmt --check`：通过
- **e2e `scenarios/basic`：40/42 通过**；**`scenarios/repair`：30/30 全部通过**
  （10 种工具调用损坏格式 × 3 模型）
- `stats.json` 确认 token 统计恢复：`completion_tokens` 累计 53185（此前恒为 0）

> **关于防封禁的说明**：本次改动去除了提示词中的注入特征，且实测同样的 e2e 强度下
> 账号未被禁言（此前旧提示词在 basic + 部分压测后即触发 `biz_code=5 user is muted`）。
> 但两次测试使用的是不同账号、不同时间点，**不足以证明因果**，仅作为正面信号。
> 若仍出现封禁，请按 `docs/development.md` 的账号章节排查。

## [0.2.9] - 2026-09-13

修复 issue #93（「输出token没显示」）—— 所有端点的 `completion_tokens` / `output_tokens`
恒为 0，同时 `finish_reason` 在流意外结束时会退化为兜底值。

### Fixed

- **`completion_tokens` 恒为 0**：`ResponseStream` 有两条产生 `Done` 的路径 —— 正常拆帧路径
  和 EOF 冲刷路径。上游在 `status=FINISHED` 之后经常直接断流（不发结尾空行），最后几帧
  （含 `accumulated_token_usage` / `response/status`）会滞留在缓冲区由 EOF 路径处理，
  而该路径拿到事件后直接返回首个事件，既没做 `status→Done` 转换也没带出 usage。
  实测上游确实下发了 `accumulated_token_usage: 87`，但对外报 `completion_tokens: 0`。
  现抽出 `finalize_events()` 供两条路径共用，EOF 分支改为循环冲刷全部残留帧
  （含无结尾空行的尾帧），并补齐收尾逻辑
- **`finish_reason` 丢失**：同一根因，EOF 路径的 `Done` 硬编码 `finish_reason: None`，
  导致 `stop` 只能靠上层兜底推断；现在能正确保留上游的 FINISHED / INCOMPLETE 语义
- **e2e 框架无法运行**：`anthropic>=1.5` 内部改用 `httpx2`，`runner.py` /
  `stress_runner.py` 传入 `httpx.Client(http_client=...)` 会直接 `TypeError`，
  整个 e2e 套件跑不起来。改为使用 SDK 自带 `timeout` 参数，依赖同步为 `httpx2`

### Added

- 4 项回归测试覆盖 usage 透传、INCOMPLETE 语义、Done 去重与事件顺序
  （已验证移除修复后其中 3 项会失败）

### 测试结果

使用真实账号完成端到端验证：

| 端点 | 修复后 usage |
|------|--------------|
| `/v1/chat/completions`（非流式） | `{prompt_tokens: 31, completion_tokens: 87}` —— 与上游 `accumulated_token_usage: 87` 一致 |
| `/v1/chat/completions`（流式 + `include_usage`） | `{prompt_tokens: 31, completion_tokens: 89}` |
| `/anthropic/v1/messages` | `{input_tokens: 31, output_tokens: 87}` |

- `cargo test --workspace`：128 passed / 0 failed
- `cargo clippy -- -D warnings`、`cargo fmt --check`：通过
- **e2e `scenarios/basic`：40/42 通过**（双端点 × 3 模型，覆盖基础对话、流式、深度思考、
  工具调用、文件/图片/文档上传、HTTP 链接）。2 项失败为上游对 expert 模型文件上传返回
  `code=7 rate limit reached`，属已知官方限制（v0.2.7 起的分块回退即为此而设），非本次改动引入

> **风控实测记录**：测试账号在跑完 basic 套件后、压测过程中被上游临时禁言
> （`biz_code=5 user is muted`，`mute_until` 约 23 小时后）。说明上游对短时间内的高频
> 请求非常敏感，建议保持「并发数 = 账号数 ÷ 2」并避免连续压测同一账号。

## [0.2.8] - 2026-09-13

本次发布合并了 web 管理面板的响应式/多语言重构（PR #103）、Windows HTTPS 与 `device_id` 风控支持（PR #104），
并从 PR #91 摘取了标签感知分块与多语言 stop 截断 panic 修复。

> **⚠️ 账号现状**：官方风控已大幅收紧，README / issue 中历史公开的测试账号已全部失效
> （`USER_IS_BANNED` / `user is muted` / `RISK_DEVICE_DETECTED`）。请使用自己的账号并按
> `docs/development.md` 配置 `device_id`，否则登录会被风控直接拒绝。

### Added

- **Web 管理面板现代化**（PR #103）
  - 三语支持：新增 Bahasa Indonesia（`web/src/locales/id/common.json`），zh/en/id 三份词条
    各 180 个 key 且完全对齐，切换器按 zh → en → id 循环
  - 响应式布局：桌面侧边栏可折叠并持久化到 `localStorage`，平板折叠为图标栏，
    移动端改为底部标签栏 + Material 3 卡片式列表
  - PWA：`manifest.json` / `manifest.webmanifest` / `sw.js`（`/admin/` 作用域，
    静态资源 stale-while-revalidate，`/admin/api/*` 直连网络）+ 启动闪屏 `SplashScreen.tsx`
  - 新增 `SettingsPage`（Server / Proxy / ds_core 参数 + 管理员密码修改）、
    `UserDropdown`（账户菜单：主题 / 语言 / 日志 / 设置 / 退出）、`CodeSnippet`（cURL / Python / Node.js 示例）
  - `lib/theme.ts` 抽出主题 hook；`lib/api.ts` 增加 `normalizeConfig` 与后端错误多语言本地化
- **账号 `device_id` 支持**（PR #104）：`AccountConfig` / `Account` / 管理面板均新增可选 `device_id` 字段，
  登录时写入 `POST /api/v0/users/login` 请求体，用于绕过 `RISK_DEVICE_DETECTED`（biz_code 11）风控；
  管理面板提交时留空会保留服务端已有值，旧前端不发送该字段也兼容
- **`wreq` 启用 `webpki-roots`**（PR #104）：`default-features = false` 时 boring2 会回退到
  `set_default_verify_paths()`，在 Windows 上找不到 CA 证书导致所有 HTTPS 请求握手失败；启用后修复
- **回归测试**：`split_prompt_chunks` 标签边界 4 项、stop 截断多语言/跨 chunk 2 项，
  均已在未修复代码上确认可复现失败
- **文档**：README / README.en.md 重写测试账号章节（风控错误码表 + `device_id` 获取步骤）；
  AGENTS.md 同步前端结构、分块策略、`device_id` 与风控排查项；`docs/development.md` 新增
  「账号准备与风控」和「Web 前端」两节（含 i18n key 覆盖自查脚本）

### Security

修复 `cargo audit` 报告的 4 个真实漏洞（此前 CI 的 audit 步骤为告警配置，未真正阻断）：

| 依赖 | 问题 | 处理 |
|------|------|------|
| `bcrypt` 0.19.1 | RUSTSEC-2026-0199：`verify` 收到非 ASCII hash 时 panic（medium） | 升级到 `0.19.3` |
| `wasmtime` 45.0.0 | RUSTSEC-2026-0269：路径/符号链接以斜杠结尾时文件系统沙箱逃逸（high） | 升级到 `48.0.2` |
| `wasmtime` 45.0.0 | RUSTSEC-2026-0222：Store 之间可能混淆类型索引（low） | 同上 |
| `crossbeam-epoch` 0.9.18 | RUSTSEC-2026-0204：无效指针的 `fmt::Pointer` 解引用 | 升级到 `0.9.21`（wasmtime 传递依赖） |

同时升级 `anyhow` 1.0.104、`rand` 0.10.2（拉起修复后的 `chacha20` 0.10.2）。

剩余 3 条为**无法在本仓库消除**的上游告警，已在 `.cargo/audit.toml` 中文档化并显式忽略：
`wreq` / `wreq-util` 5.x 被上游全部 yank（crates.io 上非 yanked 的只有 6.0.0-rc.*），
以及由 `wreq` 传递引入的 `lru` 0.13 `unsound`（RUSTSEC-2026-0253，触发前提是缓存 key 的
`Drop` panic，本项目未使用该模式，当前锁定的上游版本尚不存在 0.13 补丁）。

CI 的 audit / outdated 步骤同步修正：原 `actions-rust-lang/audit` 的 `args: --deny warnings`
不是该 action 的合法输入（日志中出现 `Unexpected input(s) 'args'`），实际并未生效；
现在改为显式执行 `cargo audit`，并新增 `scripts/check-outdated.sh` 处理 yank 导致的解析阻塞
（真实「有新版可用」仍会失败，仅跳过 wreq yank 这一已知情况）。

### Fixed

- **expert 分块切分不再切断标签**（取自 PR #91）：`split_prompt_chunks` 改为按 `<｜Role｜>` 标签边界
  贪心打包，避免切出 `<｜Assista` / `nt｜>` 半截标签导致上游偶发空回复（issue #79）；
  单个 message 超限时退化为该块的字符切分
- **多语言输出下 stop 截断 panic**（取自 PR #91）：`StopDetectStream` 中
  `&buffer[sent_len..pos]` 在 `sent_len > pos`（stop 串跨 chunk）或 byte index 落在
  UTF-8 续字节上（中文 / 日文 / 俄文）时会 panic（`begin > end when slicing`）；
  现取 `min` 并用 `floor_char_boundary` 对齐 char 边界
- **Docker 配置回归**：`docker/config.example.toml` 的 `host` 曾被同步脚本改回 `127.0.0.1`，
  导致容器内只监听环回地址、宿主机端口映射不可达；恢复为 `0.0.0.0` 并补充注释

### Changed

- `Cargo.toml` / `ds_core/Cargo.toml` 版本提升到 `0.2.8`，`web/package.json` 同步为 `0.2.8`
- 依赖升级：`wasmtime` 45 → 48、`bcrypt` 0.19.1 → 0.19.3、`anyhow` 1.0.102 → 1.0.104、
  `rand` 0.10.0 → 0.10.2、`crossbeam-epoch` 0.9.18 → 0.9.21、`chacha20` 0.10.0 → 0.10.2
- `just check` 中 `cargo audit --deny warnings` 改为 `cargo audit`，`cargo outdated` 改走
  `scripts/check-outdated.sh`；CI 同步
- `ds_core/Cargo.toml` 的 `wreq` 显式开启 `webpki-roots`（PR #104），保证 Windows 上 HTTPS 可用
- AGENTS.md 明确三语词条 key 必须保持一致，并记录 PWA / 响应式相关文件位置

### 测试结果

- `cargo test --workspace`：124 passed / 0 failed（ds-free-api 120 + ds_core 4，其中新增 6 项回归测试）
- `cargo clippy -- -D warnings`、`cargo fmt --check`：通过
- `cargo check --workspace --all-targets`：通过
- 前端 `bun install --frozen-lockfile` + `bun run typecheck` + `bun run build` + `bun run lint`：全部通过
- i18n key 覆盖：zh / en / id 各 180 key，缺失 0，三份 key 集合完全一致
- 管理面板接口手测：`/health`、`/v1/models`、`/anthropic/v1/models`、未授权 401、
  `POST /admin/api/setup` → `login` → `GET/PUT /admin/api/config` 全链路通过；
  验证了 PUT 省略 `device_id` 时服务端保留已有值
- `cargo audit`：0 vulnerabilities（修复 4 个真实漏洞后）
- **限制说明**：本次上游风控导致所有可获取的公开测试账号均被封禁或禁言
  （3 个新账号返回 `user is muted` + `mute_until`，15 个历史账号返回 `USER_IS_BANNED`），
  因此未能完成真实模型推理的 e2e 场景回归；聊天链路仅验证到账号池耗尽时正确返回
  `429 {"code":"overloaded"}` 且不 panic

## [0.2.7-pre1] - 2026-05-14

### Fix: 主要修复因为官方限制expert模型的上传文件导致的问题, 以及其他的一些修改

主要原因是网页端的单次输入有 `input_character_limits`, 所以是通过一个文件包含长上下文下的历史对话。这次官方限制了expert的文件上传, 并且是内部静默忽略不报错导致不会执行回退, 所以导致了expert的使用异常。

目前的解决方案是:

- default、vision采用原来的模式, 但是需要超过 `input_character_limits * 75 / 100` 的限制时才触发历史文件, 否则依旧一致请求;
- expert采用新的分块completion模式同样在超过限制时触发, completion_1带一部分历史, 然后立即stop_stream, 不让模型实际输出, 再开始 completion_2, 以此类推直到完成完整历史对话拼接。这样的实现感觉问题有点大, 如果有更好的想法欢迎提issue或pr。

> 还有有些账号还没有开放vision测试, 可能会导致vision请求出现空回复的问题

---

- [x] 合并并修复 PR #52，采用 Co-Authored-By 机制（Web 英文版）
- [x] 实施更严格的 Lint 检查
- [x] 补充测试账号
- [x] 将前端默认开发运行时切换为 Bun
- [x] 处理 PR #63：当 `message_start` 后上游出错时，补发 `message_delta` 与 `message_stop`，防止客户端挂死
- [x] 将变量名由 `rquest` 重构为 `wreq`，并更新相关依赖
- [x] 合并处理 PR #66
- [x] 对齐最新的流式处理逻辑，并修复若干相关问题
- [x] 合并处理 PR #67，实现前端颜色主题切换
- [ ] 实现 Issue #65 所述的 `v1/files` 端点，为视觉模型提供必要的 `model_type` 支持
- [x] 修复专家模式下的问题
- [x] 实现 `model_type: vision` 并同步更新网页端 API
  - [x] 测试 `search_enabled` 参数，确认后端是否会默认忽略
  - [x] 添加端到端测试
- [ ] 排查 Issue #58 中出现的异常字符
- [ ] 将 Issue #53 中的 `<｜End▁of▁sentence｜>` 设置为内部强制结束标记，解决模型错误生成用户回答的幻觉问题
- [ ] 处理 Issue #56 中 qwenpaw 相关问题，预计需将 OpenAI 适配器的空工具调用保活机制替换为空思考块
- [ ] 添加项目更新提醒
- [ ] 将 [DeepSeek 服务状态页](https://status.deepseek.com/) 纳入提醒列表
- [ ] 实现 response 端点

## [0.2.6] - 2026-05-05

### Added
- **Web 管理面板**：基于 Vite + React + shadcn/ui 的 SPA，含登录、Dashboard 概览页、配置编辑页。
  `PUT /admin/api/config` 统一替代旧 keys/accounts CRUD / reload / relogin 等 6 个分散端点。
  配置编辑支持 Server、DeepSeek、模型类型、工具调用标签、代理、账号、API Keys 七节编辑，
  账号和 Keys 常驻展开，其余默认折叠。
- **管理后台安全**：`auth.rs` JWT 签发/验证（HMAC + SHA256），管理员密码设置与登录，
  密码 bcrypt 哈希存储，登录频率限制。
- **Config 管理增强**：
  - 配置自动创建：配置文件不存在时自动生成最小配置写入磁盘
  - `Config::save()` 原子写入（tmp + rename + 0600 权限）
  - `Config` 改为 `Arc<RwLock<Config>>`，运行时可变，管理面板变更自动持久化
  - `DS_CONFIG_PATH` 环境变量，优先级：`-c` > `DS_CONFIG_PATH` > 默认 `config.toml`
  - 配置归并：`admin.json`、`api_keys.json` 合并到 `config.toml` 的 `[admin]` / `[[api_keys]]` 节
  - PUT 配置合并保护：密码/key 为 `***`/空值时自动保留当前值
- **Docker 部署**：`docker/Dockerfile`（alpine:3.21，musl 静态编译，~20MB 镜像）、
  `docker/docker-compose.yaml`、`docker/config.example.toml`（host = 0.0.0.0，空账号）。
  镜像发布到 ghcr.io。
- **重试全链路日志**：`try_chat()` 每次 Overloaded 退避重试输出 WARN 日志（含尝试次数和等待时间），
  重试成功输出 INFO，全部失败输出 WARN 终结日志
- **WAF 友好提示**：检测到 AWS WAF Challenge 时输出清晰的双语提示，替代原有的无意义错误
- **账号自动去重**：启动时按 email（优先）或 mobile 去重
- **`X-Client-Locale` 请求头**：DeepSeekConfig 新增 `client_locale` 字段，默认 `zh_CN`
- **代理配置**：`[proxy]` 配置项，支持 HTTP/HTTPS/SOCKS5
- **CI build-frontend 独立 job**：产物供后端 check/test 使用，确保编译嵌入真实前端文件
- **GPL-3.0 许可证**

### Changed
- **HTTP 客户端**：`reqwest`（rustls）→ `rquest`（BoringSSL + Chrome 136 TLS 指纹模拟）。
  替换后 TLS 握手指纹模拟 Chrome 136 浏览器，配合 Android 请求头绕过 WAF 指纹检测
- **默认端口**：`5317` → `22217`，避开 Win10 Hyper-V 动态端口保留区间（5000–6000）
- **默认请求头**：全面切换为 DeepSeek Android 客户端格式 ——
  `User-Agent: DeepSeek/2.0.4 Android/35`、`X-Client-Version: 2.0.4`、`X-Client-Platform: android`
- **wasmtime**：43.0.0 → 44.0.0，修复安全通告 RUSTSEC-2026-0114
- **`model_aliases` 类型**：`HashMap<String, String>` → `Vec<String>`，按 index 对齐 `model_types`
- **`/` 根路径**：从 JSON 端点列表改为 302 重定向到 `/admin`
- **stderr 彩色日志**：TRACE=紫、INFO=绿、WARN=黄、ERROR=红、DEBUG=蓝，仅终端连接时启用
- **handler/store 重构**：
  - `chat_completions` / `anthropic_messages` 统计日志提取为 `AppState::record_request()`
  - `admin_setup` / `admin_login` 从各 ~50 行压缩到 ~12 行
  - `admin_reload_config` 从 ~70 行压缩到 ~10 行
  - `StoreManager` 从读写独立 JSON 改为委托共享 `Arc<RwLock<Config>>`
- **CI 构建重构**：
  - `build-frontend` 独立 job，check/test 通过 `needs` 依赖前端产物
  - `cross` 升级到 0.2.5，aarch64-linux-gnu/musl 迁移到原生 ARM 运行器（`ubuntu-24.04-arm`）
  - `actions-rust-lang/setup-rust-toolchain` 替换 `dtolnay/rust-toolchain`
  - `just check-web` 新增前端校验命令（npm ci + build + lint）
- **过时内容清除**：
  - 移除 6 个分散管理端点（keys CRUD / accounts CRUD / reload / relogin）
  - 移除 `sse_stream()` / `SseSerializer`（流式响应全面改用 `inspect`/`map`/`TokenGuardStream`）
  - 移除 `StopStream` / repetition detection
  - 移除 `.dockerignore`、根目录 `Dockerfile` / `docker-compose.yml`
  - 移除 `web/config.toml` 等无用旧文件

### Removed
- `reqwest` 依赖
- `admin.json`、`api_keys.json` 独立文件（合并入 `config.toml`）
- 启动时 `accounts.is_empty()` 验证（无账号通过管理面板补充）
- `DS_CONFIG` 环境变量（由 `DS_CONFIG_PATH` 替代）
- `web/config.toml`

### Fixed
- **CI 幂等性**：`cargo install` 步骤添加 `command -v` 前置检查
- **client.rs 日志违规**：`print_waf_hint()` 中 11 条 `warn!` 补全 target 参数
- **stats.json 空文件**：不再触发 EOF 解析 WARN，降级为 INFO
- **e2e 端口硬编码**：runner.py / stress_runner.py 改为从 config.toml 动态读取端口
- **AGENTS.md 过时内容**：`/` 端点描述、`[[server.api_tokens]]` → `[[api_keys]]`、WASM 故障排查等

### Docs
- **README / README.en.md**：新增环境变量表格；设计哲学补充"非必要不引入额外运行时系统依赖"；管理面板截图
- **`docs/en/`**：英文文档目录，所有文档提供英文版
- **`docs/development.md` / `docs/en/development.md`**：构建、Docker、e2e 测试开发指南
- **Prompt injection 策略**：更新 README 中 DeepSeek 原生标签注入策略说明
- **CLAUDE.md / AGENTS.md**：架构描述精简，新增故障排除表、请求追踪 grep 示例、`#[allow]` 策略说明

## [0.2.5] - 2026-04-30

### Added
- **文件上传**：支持通过 API 上传文件/图片到 DeepSeek。OpenAI 端点的 `file` / `image_url` content part
  和 Anthropic 端点的 `document` / `image` content block 均可使用。内联 data URL 自动上传，
  HTTP URL 触发搜索模式，由模型自行访问
- **XML `<invoke>` 格式原生解析**：直接解析 `<invoke name="..."><parameter>` 格式的工具调用，
  无需触发修复管道，响应更快
- **流式工具调用保活**：模型生成工具调用期间（通常 2–10s），每 1s 发送空增量块防止客户端超时。
  OpenAI 端点为空 `tool_calls` delta，Anthropic 端点为 `"tool_calls..."` thinking 块
- **工具调用标签用户自维护**：`config.toml` 新增 `[deepseek.tool_call]` 配置项，
  用户可随时追加新发现的模型幻觉标签，无需等待代码更新

### Changed
- **Prompt 格式升级**：从 ChatML（`<|im_start|>` / `<|im_end|>`）全面迁移到 DeepSeek 原生标签格式。
  每次 `<｜User｜>` 前插入 `<｜end▁of▁sentence｜>` 闭合上一轮；工具结果改用 `<｜tool▁outputs▁begin｜>` 包裹；
  reminder 嵌入 `<think>` 块。与 DeepSeek 官方 chat_template 对齐后，模型遵循度明显提升
- **工具调用主标签变更**：从 `<|tool_calls_begin|>` 改为 `<|tool▁calls▁begin|>` / `<|tool▁calls▁end|>`
  （使用 ASCII `|` + `▁`）。模型输出这个标签的概率大幅高于旧标签，幻觉变体明显减少。
  默认回退标签覆盖已知变体：`<|tool_calls_begin|>`、`<|tool▁calls_begin|>`、`<|tool_calls▁begin|>`、`<tool_call>`
- **智能搜索默认开启**：搜索模式下 DeepSeek 注入的系统提示词更强，能提升工具调用遵循度

### Fixed
- **Anthropic 协议兼容性**：`message_start` 补回 `stop_reason: null` / `stop_sequence: null`；
  `message_delta` 始终携带 `usage.output_tokens`；usage 不再始终为 0。
  以上修复解决 Claude Code 等标准 Anthropic 客户端的兼容性问题
- **文件上传错误处理**：历史对话文件上传失败时自动回退为内联 prompt，不再静默丢失上下文；
  外部文件上传失败直接返回明确错误，不再静默跳过
- **修复模型准确度**：自修复请求现在自动携带工具定义列表和 JSON 转义提示，
  模型从破碎文本推测正确参数的能力明显提升

## [0.2.4] - 2026-04-27

### Added
- **历史对话文件化**：多轮对话历史自动拆分上传为独立文件，绕过 DeepSeek 单次输入长度限制。
  对适配器层完全透明，上传失败不影响主流程，自动退化为纯文本发送
- **临时 Session 生命周期**：每次请求创建独立 session，请求结束自动清理（stop_stream + delete_session），
  彻底杜绝 session 泄漏和 TTL 过期残留
- **工具调用自修复**：当模型输出的 tool_calls 格式异常时，使用 DeepSeek 自身修复损坏的 JSON/XML，
  流式和非流式路径均覆盖，大幅提升工具调用成功率
- **arguments 类型归一**：自动处理 arguments 为 JSON 字符串的异常情况，避免客户端双重转义解析失败
- **`input_exceeds_limit` 检测**：识别输入超长错误并返回明确错误信息，不再静默失败
- **全链路日志追踪**：`req-{n}` 标识贯穿 handler → adapter → ds_core 全层，
  `x-ds-account` 响应头标识处理账号，单次请求可完整 grep 追踪
- **TRACE 级别字节追踪**：流管道各层 TRACE 日志，可观察字节在 SSE 管道中的完整转换过程
- **`/` 端点**：免鉴权返回可用端点列表和项目地址
- **e2e 测试重构**：从 pytest 迁移为 JSON 场景驱动框架，场景独立存放，配置动态读取

### Changed
- **请求流程重构**：从"持久 session + edit_message"升级为"临时 session + completion + 文件上传"，
  每次请求独立生命周期，不再依赖预创建的持久 session
- **限流自动重试**：检测到 rate_limit 时以 1s→2s→4s→8s→16s 指数退避自动重试（最多 6 次），
  对用户透明，大幅降低限流导致的请求失败
- **Prompt 构建优化**：reminder 插入位置调整到最后一轮对话之前，确保模型优先遵循指令；
  工具描述的代码块格式化；工具调用结果的 Markdown 结构化展示
- **推理控制语义修正**：禁用思考时使用 `"none"` 替代 `"minimal"`，语义更明确
- **日志级别规范化**：账号池耗尽提升为 `WARN`，常规分配降为 `DEBUG`，
  新增 session/上传/PoW 等 debug 日志，health_check 合并为单条带耗时日志

### Removed
- 账号初始化不再按 model_type 管理 session，移除 session 持久化和 update_title 逻辑
- 移除旧 pytest e2e 测试目录（被 JSON 场景驱动框架替代）

### Test Results

#### py-e2e-tests
- **4 账号 + 3 并发 + 3 迭代**：17 场景 × 2 模型 × 3 次 = 102 次请求，成功率 100%，总耗时 5.5 分钟
- 覆盖场景：基础对话、深度思考、流式、标准工具调用，以及 10 种 tool_calls 损坏格式
  （XML/JSON 混合、字段名不一致、arguments 字符串、括号不匹配/缺失、
  name/arguments 互换、参数外溢等），修复管道全部正确兜底

#### claude-code 测试
```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:5317/anthropic
export ANTHROPIC_AUTH_TOKEN=sk-test
export ANTHROPIC_DEFAULT_OPUS_MODEL=deepseek-expert
export ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-expert
export ANTHROPIC_DEFAULT_HAIKU_MODEL=deepseek-default
claude
```
- 基本稳定, 工具解析时会使得claude-code暂时卡住是正常现象, 部分情况可能出现模型不遵循指令导致工具调用指令泄漏
- 其他编程工具没有大量测试, 希望大家积极反馈

## [0.2.3] - 2026-04-24

### Added
- Tool call XML 解析增强：增加 `repair_invalid_backslashes` 与 `repair_unquoted_keys`
  宽松修复，当模型输出的 JSON 包含未引号 key 或无效转义时自动修复后重试
- 增加 `is_inside_code_fence` 检查：跳过 markdown 代码块中的工具示例，防止误解析
- 新增 Anthropic 协议压测脚本 `stress_test_tools_anthropic.py`，与 OpenAI 版对称
- 示例文件正交化：`examples/adapter_cli/` 下按功能拆分为
  `basic_chat`/`stream`/`stop`/`reasoning`/`web_search`/`reasoning_search`/`tool_call` 等独立文件
- 默认 adapter-cli 配置文件路径指向 `py-e2e-tests/config.toml`

### Changed
- 账号池选择策略：从**轮询线性探测**改为**空闲最久优先**，最大化账号复用间隔
- 移除固定的冷却时间常量，选择算法天然避免账号被过快重用
- 同步更新中英文 README，增加并发经验说明

### Stress Test Results

针对 4 账号池的 70 请求压测（7 场景 × 2 模型 × 5 迭代）：

| 策略 | 并发 | 成功率 | 平均耗时 |
|------|------|--------|----------|
| 轮询 + 无冷却 | 3 | 25.7% | 2.57s |
| 轮询 + 2s 冷却 | 3 | 97.1% | 10.46s |
| **空闲最久优先 + 无冷却** | **2** | **100%** | **10.14s** |
| **空闲最久优先 + 无冷却 (Anthropic)** | **2** | **100%** | **11.31s** |

结论：稳定安全并发 ≈ 账号数 ÷ 2，空闲最久优先策略可在不设冷却的前提下实现 100% 成功率。

## [0.2.2] - 2026-04-22

### Added
- Anthropic Messages API 兼容层：
  - `/anthropic/v1/messages` streaming + non-streaming 端点
  - `/anthropic/v1/models` list/get 端点（Anthropic 格式）
  - 请求映射：Anthropic JSON → OpenAI ChatCompletion
  - 响应映射：OpenAI SSE/JSON → Anthropic Message SSE/JSON
- OpenAI adapter 向后兼容：
  - 已弃用的 `functions`/`function_call` 自动映射为 `tools`/`tool_choice`
  - `response_format` 降级：在 ChatML prompt 中注入 JSON/Schema 约束（`text` 类型为 no-op）
- CI 发布流程改进：
  - tag 触发 release（`push.tags v*`）
  - CHANGELOG 自动提取版本说明
  - 发布前校验 Cargo.toml 版本与 tag 一致

### Changed
- Rust toolchain 升级到 1.95.0，CI workflow 同步更新
- justfile 添加 `set positional-arguments`，安全传递带空格的参数
- Python E2E 测试套件重组为 `openai_endpoint/` 和 `anthropic_endpoint/`
- 启动日志显示 OpenAI 和 Anthropic base URLs
- README/README.en.md 添加 SVG 图标、GitHub badges、同步文档
- LICENSE 添加版权声明 `Copyright 2026 NIyueeE`
- CLAUDE.md/AGENTS.md 同步更新

### Fixed
- Anthropic 流式工具调用协议：使用 `input_json_delta` 事件逐步传输工具参数
- Tool use ID 映射一致性：`call_{suffix}` → `toolu_{suffix}`
- Anthropic 工具定义兼容：处理缺少 `type` 字段的情况（Claude Code 客户端）

## [0.2.1] - 2026-04-15

### Added
- 默认开启深度思考：`reasoning_effort` 默认设为 `high`，搜索默认关闭。
- WASM 动态探测：`pow.rs` 改为基于签名的动态 export 探测，不再硬编码 `__wbindgen_export_0`，降低 DeepSeek 更新 WASM 后启动失败的风险。
- 新增 Python E2E 测试套件：覆盖 auth、models、chat completions、tool calling 等场景。
- 新增 `tiktoken-rs` 依赖，用于服务端 prompt token 计算。
- CI 新增 `cargo audit` 与 `cargo machete` 检查。

### Changed
- 账号初始化优化：日志在手机号为空时自动回退显示邮箱。
- 更新 `axum`、`cranelift` 等核心依赖至最新 patch 版本。
- Client Version 保持与网页端一致的 `1.8.0`。

### Removed
- 移除未使用的 `tower` 依赖。

## [0.2.0] - 2026-04-13

### Added
- 项目从 Python 全面重构到 Rust，带来原生高性能和跨平台支持。
- OpenAI 兼容 API（`/v1/chat/completions`、`/v1/models`）。
- 账号池轮转 + PoW 求解 + SSE 流式响应。
- 深度思考和智能搜索支持。
- Tool calling（XML 解析）。
- GitHub CI + 多平台 Release（8 目标平台）。
- 兼容最新 DeepSeek Web 后端接口。

# 开发指南

## 环境要求

- Rust **1.95.0+**（见 `rust-toolchain.toml`）
- Bun **1.3+**（Web 面板构建与开发）
- `cmake`、`g++`、`libclang-dev`（编译 `wreq` 依赖的 BoringSSL）
- `just` 命令运行器（用于 `just serve` / `just check` 等快捷命令）

## 账号准备与风控（重要）

登录 `POST /api/v0/users/login` 会经过 DeepSeek 的风控校验，常见失败码：

| biz_code | biz_msg | 说明 | 处理方式 |
|----------|---------|------|----------|
| `10` | `USER_IS_BANNED` | 账号被永久封禁 | 不可恢复，注册新账号 |
| `5` | `user is muted` | 临时禁言，响应 `biz_data.mute_until` 为解封时间戳（通常数周） | 重登无效，health_check 会失败并把账号置为 `invalid`，只能等解封或换号 |
| `11` | `RISK_DEVICE_DETECTED` | 缺设备指纹，登录被风控拦截 | 为该账号配置 `device_id` |
| `2` | `PASSWORD_OR_USER_NAME_IS_WRONG` | 账号或密码错误 | 核对凭据 |

> 禁言（biz_code=5）在**登录响应的 `user.chat.is_muted` / `mute_until`**
> 中即可见，`ds_core` 初始化时据此做早检，命中即不再创建 session /
> 发送 health_check completion。

### 获取并配置 `device_id`

`device_id` 由数美（Shumei）SDK 在浏览器中生成。它是**设备级**指纹，上游用它做关联与画像，
因此**强烈建议每个账号使用各自独立的 `device_id`**（例如每个账号单独开一个无痕窗口 /
浏览器配置文件登录一次）。把同一个值复用到多个账号会显著提高被风控关联的风险：

1. 用 Chrome 打开 `https://chat.deepseek.com/sign_in`，登录一次，等待页面完全加载
2. 开发者工具 → Network，过滤 `users/login`，发起登录后查看该请求的 Payload，复制 `device_id`
3. 或直接在控制台执行 `SMSdk.getDeviceId()`（需等 SDK 就绪）
4. 对**每个账号**重复上述步骤（各自独立的浏览器配置文件），分别写入配置：

```toml
[[ds_core.accounts]]
email = "you@example.com"
mobile = ""
area_code = ""
password = "your-password"
device_id = "抓取到的值"
```

> ⚠️ `device_id` **不能伪造**：实测伪造的 base64（88 字符）或普通字符串都会返回
> `RISK_DEVICE_DETECTED`（biz_code 11），必须来自真实浏览器中 SDK 生成的值。
> 因此「每账号独立指纹」需要为每个账号各自注册一次设备，这是一项真实成本。

管理面板 → 配置页也有该字段；提交时留空（或旧前端不发送该字段）会保留服务端已有值，
因此升级后既有配置无需改动。

> 提醒：官方近期对共享账号封禁力度很大，公开测试账号基本已全部失效，
> 请使用自己的账号并在多账号间保持合理并发（推荐并发数 = 账号数 ÷ 2）。

## 首次启动

```bash
# 1. 复制配置
cp config.example.toml config.toml

# 2. 构建 Web 前端（编译时嵌入二进制，每次前端变更需要重构建）
cd web && bun install && bun run build && cd ..

# 3. 运行开发服务器
just serve
```

服务器启动后访问 `http://localhost:22217` 自动跳转到管理面板。

> **前端热更新开发**：同时运行 `cd web && bun run dev`（Vite HMR 模式）
> 和 `just serve`，后端优先使用文件系统 `web/dist/` 目录中的静态文件。
> 无需每次前端改动都重构建二进制。

## Release 构建

```bash
# 1. 构建 Web 前端
cd web && bun install && bun run build && cd ..

# 2. 构建 Release 二进制
cargo build --release

# 3. 运行（也可直接运行二进制，无需 web/dist/ 目录）
./target/release/ds-free-api
```

Release 二进制通过 `rust_embed` 编译时嵌入前端资源，`web/dist/` 目录不存在时
自动使用嵌入资源。发布版无需额外文件。

## CI 自动构建

GitHub Actions（`.github/workflows/release.yml`）在 tag push 时自动执行：

```
build-frontend (bun install --frozen-lockfile + bun run build)
  ├── build-linux-gnu (cargo build)     │
  ├── build-linux-musl (musl-cross)     │── release (tar.gz + zip)
  ├── build-macos (cargo build)  │
  └── build-windows (cargo build)│
  └── docker (ghcr.io image)
```

`build-frontend` 产出 `web-dist` artifact，各编译 job 下载后再执行 `cargo build` /
`cross build`，保证 `rust_embed` 嵌入真实前端文件。

Docker 镜像自动推送到 `ghcr.io/niyueee/ds-free-api:latest`。

## Docker 部署（生产）

从 ghcr.io 拉取（推荐）：

```bash
# 确认已创建 docker/config/ 目录（自动创建或手动 mkdir）
docker compose -f docker/docker-compose.yaml up -d
```

容器首次启动时自动创建最小配置，无需提前准备 `config.toml`。
配置和数据通过 bind mount 持久化到宿主机的 `docker/config/` 和 `docker/data/`。

从源码构建本地 Docker 镜像：

```bash
# 1. 构建前端 + 交叉编译二进制
cd web && bun install && bun run build && cd ..
cargo zigbuild --release --target x86_64-unknown-linux-gnu

# 2. 构建 Docker 镜像
docker build -f docker/Dockerfile -t ds-free-api .

# 3. 导出并传输到服务器
docker save ds-free-api | gzip > ds-free-api.tar.gz
scp ds-free-api.tar.gz user@server:/tmp/

# 4. 服务器加载并启动
ssh user@server
docker load < /tmp/ds-free-api.tar.gz
docker compose -f docker/docker-compose.yaml up -d
```

> 服务器原生 x86 环境可直接在服务器上执行上述构建，速度更快。
> Docker 镜像仅包含预编译二进制 + 嵌入的前端资源，无需在容器内编译。

## 命令参考

```bash
# 一键检查（check + clippy + fmt + audit + unused deps）
just check

# 运行测试
cargo test --lib

# 运行 HTTP 服务
just serve

# 统一协议调试 CLI（内置对话/比较/并发等模式）
just adapter-cli

# 使用 e2e 专属配置启动服务
just e2e-serve
```

## Web 前端

Vite + React + shadcn/ui，位于 `web/`，构建产物由 `rust_embed` 在编译期嵌入二进制。

```bash
cd web
bun install --frozen-lockfile
bun run typecheck   # tsc -b
bun run build       # 产物输出到 web/dist/
bun run lint        # eslint
```

本地联调时推荐同时运行 `bun run dev`（Vite HMR）与 `just serve`；后端检测到文件系统存在
`web/dist/` 时优先从磁盘读取，改动无需重新编译 Rust。

### 目录约定

- `src/pages/`：`DashboardPage` / `ConfigPage` / `SettingsPage` / `LogsPage` / `ModelsPage` / `LoginPage` / `Layout`
- `src/components/`：`LanguageSwitcher`、`ThemeSwitcher`、`UserDropdown`、`CodeSnippet`、`SplashScreen`
- `src/lib/`：`api.ts`（全部管理端点 + `normalizeConfig` / `localizeAuthError`）、`auth.tsx`、`theme.ts`
- `src/locales/{zh,en,id}/common.json`：三语词条

### i18n 约定

新增文案时必须**同时**修改 `zh` / `en` / `id` 三个文件，保持 key 完全一致。
`ConfigPage.tsx` 里曾因引用 `config.ds_core.accounts.*`（而词条在 `config.accounts.*`）
导致页面直接显示原始 key，提交前建议自查一遍：

```bash
python3 - <<'PY'
import json,re,glob
used=set()
for f in glob.glob('web/src/**/*.tsx',recursive=True)+glob.glob('web/src/**/*.ts',recursive=True):
    used |= set(re.findall(r"\bt\(\s*['\"]([^'\"]+)['\"]", open(f,encoding='utf-8').read()))
def flat(d,p=''):
    out=set()
    for k,v in d.items():
        nk=f"{p}.{k}" if p else k
        out |= flat(v,nk) if isinstance(v,dict) else {nk}
    return out
for loc in ('en','zh','id'):
    have=flat(json.load(open(f'web/src/locales/{loc}/common.json')))
    print(loc, 'missing:', sorted(used-have))
PY
```

### 响应式与 PWA

- 侧边栏折叠状态存 `localStorage`（key `ds-sidebar-collapsed`），平板折叠为图标栏，移动端使用底部标签栏
- `public/sw.js` 由 `index.html` 在 `/admin/` 作用域注册：静态资源 stale-while-revalidate，
  `/admin/api/*` 直连网络（离线时返回 JSON 错误而非缓存）
- `web/e2e/capture-responsive.ts` 是 Playwright 截图脚本，需要本地 22217 端口有服务在跑：

  ```bash
  cd web && npx playwright install chromium
  node e2e/capture-responsive.ts
  ```

## e2e 测试

`py-e2e-tests/` 是基于 JSON 场景驱动的端到端测试框架，无需 pytest 依赖。分为三层：

| 层级       | 命令              | 覆盖范围                                              |
| ---------- | ----------------- | ----------------------------------------------------- |
| **Basic**  | `just e2e-basic`  | 基础功能场景（双端点 OpenAI + Anthropic），安全并发数 |
| **Repair** | `just e2e-repair` | 工具调用异常格式修复专项（OpenAI 单端点），安全并发数 |
| **Stress** | `just e2e-stress` | 全部场景 × 3 次迭代，安全并发数 + 1 并发              |

先启动服务端：

```bash
just e2e-serve
```

再在另一个终端运行 e2e 测试：

```bash
# 基础场景测试
just e2e-basic

# 工具修复测试
just e2e-repair
```

场景文件在 `scenarios/` 中按类型独立存放：

```
py-e2e-tests/
├── scenarios/
│   ├── basic/
│   │   ├── openai/         # 7 个基础场景（对话、推理、流式、工具调用、文件上传、图片上传、HTTP链接）
│   │   └── anthropic/      # 7 个基础场景（对话、推理、流式、工具调用、文档上传、图片上传、HTTP链接）
│   └── repair/             # 10 个工具损坏格式场景
├── runner.py               # 单次运行入口
├── stress_runner.py        # 多迭代压测入口
└── config.toml             # e2e 专用服务端配置
```

每个场景为独立 JSON 文件，包含请求参数和校验规则：

```json
{
  "name": "场景名称",
  "endpoint": "openai|anthropic",
  "category": "basic|repair",
  "models": ["deepseek-default", "deepseek-expert", "deepseek-vision"],
  "messages": [{"role": "user", "content": "..."}],
  "tools": [...],
  "tool_choice": "auto",
  "request": {"stream": false},
  "checks": {
    "has_tool_calls": true,
    "tool_names": ["get_weather"],
    "finish_reason": "tool_calls",
    "no_error": true
  }
}
```

### e2e CLI 参数

**`just e2e-basic` 和 `just e2e-repair`（单次运行）：**

| 参数 | 作用 |
|------|------|
| `scenario_dir` | 场景目录，如 `scenarios/basic` 或 `scenarios/repair` |
| `--endpoint` | 端点过滤：`openai` / `anthropic` |
| `--model` | 模型过滤：`deepseek-default` / `deepseek-expert` |
| `--filter` | 场景名称关键字过滤（多个用空格分隔，如 `--filter 文件 图片`）|
| `--parallel` | 并行数，默认 `账号数 ÷ 2` |
| `--show-output` | 显示模型回复摘要、工具调用、结束原因 |
| `--report` | 输出 JSON 报告路径 |

**`just e2e-stress`（压测）：**

| 参数 | 作用 |
|------|------|
| `--iterations` | 每场景迭代次数，默认 3 |
| `--models` | 模型列表过滤 |
| `--filter` | 场景名称关键字过滤（多个用空格分隔）|
| `--parallel` | 并行数，默认 `账号数 ÷ 2 + 1` |
| `--show-output` | 显示模型输出 |
| `--report` | 输出 JSON 报告路径 |

使用示例：

```bash
# 快速验证新加的文件上传场景
just e2e-basic --filter 文件 图片 --show-output

# 仅查看 OpenAI 端点的 expert 模型
just e2e-basic --endpoint openai --model deepseek-expert

# 串行调试
just e2e-basic --endpoint openai --parallel 1 --show-output

# 压测：工具调用修复场景 × 5 次迭代
just e2e-stress --filter 修复 --iterations 5

# 输出 JSON 报告
just e2e-basic --report result.json
```

## 更多文档

- [代码规范](code-style.md)
- [日志规范](logging-spec.md)
- [Prompt 注入策略](deepseek-prompt-injection.md)

## 实测记录：`device_id` 必须是**真实注册**的指纹（2026-09-13 A/B 验证）

**同一账号**（`v.s.i.gs.i.ehv.di.d.o.d@gmail.com`，未封禁）分别用三种 `device_id` 登录：

| device_id | 结果 |
|---|---|
| 真实浏览器注册的指纹 | 通过设备校验 → 到达 `muted` 检查（说明设备校验**已通过**）|
| 伪造的 base64（88 字符） | ❌ `RISK_DEVICE_DETECTED`（biz_code=11）|
| 伪造的普通字符串 | ❌ `RISK_DEVICE_DETECTED`（biz_code=11）|

**结论：`device_id` 不能伪造，必须是真实注册过的指纹。**

> 排查提示：不要在**已封禁**的账号上验证这一点 —— 封禁检查可能先于设备校验返回
> `USER_IS_BANNED`，会让人误以为「伪造的 device_id 也通过了」。必须用未封禁账号做 A/B。

**这对缓解措施的影响**：「每账号独立 `device_id`」意味着必须**为每个账号各自注册一次设备**
（独立浏览器配置文件 / 无痕窗口），不能靠生成随机值糊弄。这是一项真实成本。

## 实测记录：`device_id` 是必填项（2026-09-13 验证）

不带 `device_id` 发起登录会被风控直接拒绝：

```
客户端错误: Business error: code=11, msg=RISK_DEVICE_DETECTED
```

即使密码正确、账号未禁言也一样。因此 `[[ds_core.accounts]]` 的每个账号都必须填写
`device_id`（获取方式见本文件上文与 `config.example.toml`）。

## 风控观察：请求强度与禁言的关系（含最终结论）

### 完整实测时间线（同一账号 `1460183479@qq.com`，UTC）

| 时间 | 事件 | 当时状态 |
|------|------|----------|
| 11:14 | 首次成功推理 | ✅ |
| 11:23 | 累计 **186** 请求（120 次成功推理，覆盖全场景） | ✅ **未禁言** |
| 12:11 | 累计 **216** 请求后健康检查 | ✅ **未禁言** |
| 12:32 | v0.4.0 发布物冒烟测试 | ❌ **已禁言**（`mute_until` = 09-16 12:16 UTC，约 3 天） |

### 关键结论：先前的「未被禁言」是**时间受限**的

12:11 → 12:32 这 21 分钟内，**我没有产生任何有意义的流量**
（只做了一次 health_check：login + create_session + 1 completion + delete_session），
账号却在此期间被禁言。

这说明两件事：

1. **禁言是延迟判定的**，不是「跑到某个请求数就立刻封」
2. 因此「跑到 N 个请求还没被封」**不能**用来证明某个改动规避了风控 ——
   我在此前版本的文档里把这种观察写成正面信号，是**过度解读**，现已更正

### 对各类假设的重新评估

| 假设 | 证据强度 | 说明 |
|------|----------|------|
| prompt 注入格式（未闭合 `<think>` / 元指令 / 重复块） | **弱** | v0.2.10 的「同强度未被禁言」是不同账号、不同时点的对比；本次 ChatML 规范下仍被禁言，说明 prompt 格式**不是唯一或决定性因素** |
| 请求总量 / 频率 | **中** | v0.2.9 的禁言发生在压测期间；本次是低并发单账号累计 216 请求后延迟禁言。总量显然相关，但阈值与时延未知 |
| `device_id` 指纹关联 | **未知但值得警惕** | 该 `device_id` 已先后关联 3 个账号，其中 2 个曾被/正被禁言。设备级指纹很可能被上游用于关联与画像 |
| 每请求 create/delete session（短命会话） | **未证实** | 见下节；因存在跨用户泄漏风险，未做改动 |

### 已实施的缓解措施（v0.4.0）

针对上面唯一有证据支持的杠杆（请求量 + 指纹关联）：

1. **单账号每小时请求配额** `hourly_request_quota`（默认 60，0 = 不限制）
   - 账号维度的固定窗口计数；用尽的账号在本小时内不再被分配
   - 由池中其他账号承接；全部用尽时返回 429，而不是继续硬打上游
   - 账号状态接口与管理面板会显示「本小时已用 / 已用尽」
   - 默认 60 只是保守上限：单账号仍够常规交互（约每分钟 1 次）；
     但 2026-09-17 实测显示**约 27 次请求也可能触发禁言**（见下），配额不构成安全保证

2. **共用 `device_id` 启动告警**
   - 检测到多个账号共用同一指纹时，在日志中列出涉及账号并给出修复建议
   - 不阻止启动（避免破坏既有配置），但保证风险可见

3. **`device_id` 文档更正**：从「设备级可复用」改为「**每账号独立**」

### 实务建议

- **不要在单一账号上连续压测**；把请求分散到多个账号，并遵守「并发 = 账号数 ÷ 2」
- 每个账号使用**独立**的 `device_id`（各自一个浏览器配置文件 / 无痕窗口登录一次）
- 出现 `biz_code=5` 后**立即停止**使用该账号，等待 `mute_until` 到期；
  继续重试不会加速解禁，反而可能延长
- 接受一个现实：**本代理无法保证账号不被风控**，只能降低触发概率。
  需要稳定性请使用官方 API

#### 2026-09-17 验证结果：配额内仍被禁言

三个账号解禁后**逐号单独**验证（每号独立启动，跑一轮 basic + repair，
约 27 次上游请求，远低于 60 次/小时配额）：

| 账号 | 初始化 | e2e | 复查 |
|------|--------|-----|------|
| `l3366599051@163.com` | 04:08 ✅ | 04:09–04:12（13/14 + 10/10） | **04:21 已禁言**（`mute_until` ≈ 09-26 04:18） |
| `1460183479@qq.com` | 04:12 ✅ | 04:14–04:17（13/14 + 10/10） | **04:21 已禁言**（`mute_until` ≈ 09-26 04:18） |
| `n1yu3@proton.me` | 04:17 ✅ | 04:19–04:21（14/14 + 10/10） | 04:29 复查正常 → **06:58 已禁言**（`mute_until` ≈ 09-26 04:34） |

> 三次 basic 中仅有的失败均为上游 `code=7, rate limit reached` 的文件上传限流
> （重试 3 次后仍失败），与 prompt 注入无关；default 模型的对话 / 工具 / 流式 / 推理场景全部通过。

结论与局限：

- **三个账号最终全部被禁言**：账号 1、2 跑完数分钟内被禁言，账号 3 在 04:29 复查时仍正常、
  06:58 复查已禁言（`mute_until` ≈ 09-26 04:34，判定时间比账号 1、2 晚约 15 分钟）；
- **禁言是延迟判定的**：短时间内「仍然正常」不能作为安全证据；
- 本次实验三个账号**共用同一个真实 `device_id`**（当前环境直连被 AWS WAF 拦截，
  无法为每个账号各生成一个真实指纹），且跑的是同一份注入负载 —— 因此**无法区分**
  「共用指纹」与「当前注入 / 请求行为」的影响，两者都不能排除；
- 下一步的正确实验：为每个账号各注册一个真实 `device_id`（消除指纹混杂）后按上表流程重跑。
  在该混杂因素消除之前，「配额 + 标准 ChatML 注入」都不能称为已验证的安全策略。

## 2026-10-07：抓包发现缺失的风控令牌 `x-hif-leim`（本轮防封号核心）

前面所有缓解措施（配额、prompt 格式、并发、每账号 `device_id`）都只是「降低可疑度」，
因为它们都在**猜**上游看什么。这一轮换了个做法：用 Playwright 跑**真实浏览器 + 完整对话流程**，
把真实客户端发出的每一个请求逐字段拉出来和 `ds_core` 对比 —— 结果发现一个此前完全缺失的头部。

### 抓包事实（真实浏览器，`chat.deepseek.com`）

**1. 启动时序**（登录前后）：

```
GET  /api/v0/client/settings?did=<uuid>&scope=main|model|web_upgrade|provider|banner
POST /api/v0/users/login                {email,mobile:"",password,area_code:"",device_id,os:"web"}
POST /api/v0/users/auth_token/check_device
POST /api/v0/chat_session/create        → 会话在登录后立即创建并**长期复用**
GET  /api/v0/chat_session/fetch_page?lte_cursor.pinned=false
POST /api/v0/client/settings/report
```

**2. 风控令牌端点**（**无鉴权**，前端源码中由 `addSSEHeader` 注入 SSE 请求）：

```
GET  https://hif-leim.deepseek.com/query     ← 真实客户端启动即轮询，之后按 TTL 续取
→ {"code":0,"data":{"biz_code":0,"biz_data":{"value":"<base64>.<base64>"}}}
  响应头 x-hif-ttl: 600（秒）
```

**3. completion 请求**（唯一带该头的请求）：

```json
{"chat_session_id":"...","parent_message_id":null,"model_type":"default",
 "prompt":"...","ref_file_ids":[],"thinking_enabled":false,"search_enabled":true,
 "action":null,"preempt":false}
```

请求头（除常规 `x-client-*` 外）：

```
x-ds-pow-response: <PoW 解>
x-hif-leim: <来自 hif-leim.deepseek.com 的令牌>
```

### 为什么这能解释此前的全部现象

- `/chat/completion` 上**没有** `x-hif-leim` ⇒ 上游不需要任何行为统计，就能直接判定
  请求不是官方客户端发的；
- 这解释了「每账号仅 2 次请求、间隔 6–10 秒、独立 `device_id` 仍被禁言」（issue #112）；
- 也解释了「改 prompt 格式 / 降配额 / 换 UA 都没用」——这些都不是判定依据；
- 第三方反代项目（本仓库、Python 重写版等）全部漏掉了这个头，所以**所有**反代都在被禁言。

### 本轮实现（`ds_core`）

1. `ds_core/src/accounts/hif.rs`：按 `x-hif-ttl` 缓存令牌，到期前 30s 刷新，
   失败退避 60s；账号初始化时预热（对应真实客户端的启动轮询）；
   取令牌失败不阻断业务请求（真实客户端轮询失败时同样只是不带该头）。
2. `x-hif-leim` 只加在 completion / edit_message（即 SSE 请求）上，与真实客户端一致。
3. 新配置项 `hif_enabled`（默认 `true`，**仅用于对照实验**）。
4. 顺带修掉两个「全局常量指纹」：
   - `client_device_id` 留空时改为**首启生成随机 UUID 并写回配置**（此前按 `api_base`
     派生 ⇒ 所有部署共用同一个 X-Device-Id）；
   - health_check 的固定提示词「只回复\`Hello, world!\`」改为中性提示词池随机取一条。
5. 登录 payload 与真实客户端对齐：`email` / `mobile` / `area_code` 固定发送（空值发空串）。
6. 新增两个诊断入口：`cargo run --example hif_probe`（只探测风控令牌端点，
   **不产生任何账号流量**）、`cargo run --example account_check -- -c config.toml`
   （只做一次登录，读 `user.chat.is_muted`）。

### 仍然存在的、尚未解决的差异（后续方向）

| 差异 | 说明 |
|------|------|
| **启动时的 health_check completion** | 抓包显示真实客户端启动只做「登录 → check_device → 建会话 → fetch_page → settings」，**不发任何消息**；本代理每次启动都会为每个账号建会话并**发一条 completion**（文案已随机化，但模式仍是「刚登录就发消息」）。候选改法：把健康检查降级为只读探测（或默认关闭），但需先有干净账号能对照验证 |
| **会话生命周期** | 真实客户端一个会话长期复用、几乎不删除；本代理仍是「一次请求 = 建会话 → 发一条 → 立刻删」。历史上评估过 session 复用（跨用户泄漏风险 + 无上游清理接口）后放弃 |
| **X-Device-Id 粒度** | 真实客户端是「一个浏览器 profile = 一个 `device_id`（数美）+ 一个 X-Device-Id」，1:1 配对；本代理目前 X-Device-Id 是**实例级**（所有账号共用），更彻底的做法是**按账号**派生/配对 |
| **TLS 指纹与身份** | 目前是「安卓 App UA + Chrome136 TLS 指纹」。`wreq-util` 有 OkHttp 拟态档位，理论上更自洽；但桌面 Chrome UA 会被 AWS WAF 202 拦截，改动需实测 |
| **`/client/settings*` 系列请求** | 真实客户端启动会拉 5 个 scope（`did` 用 `__ds_remote_feature_did`）并在设置变更时 `report`；本代理完全不发 |
| **文件上传请求头** | 真实客户端上传时额外带 `x-thinking-enabled` / `x-model-type` / `x-file-size`；本代理未发（只影响超长 prompt / 附件路径） |
| **通用请求头细节（已实测取证，见下节）** | 当前发出的请求是「安卓 App UA + Chrome/macOS client hints + 文档导航头」的混合体，任何真实客户端都不会长这样。未直接改动是因为换身份有 WAF 风险且暂时没有干净账号做端到端验证 |

### 2026-10-07 补充取证：我们实际发出的头 vs 真实客户端

用本地 echo server（`HifConfig.leim_url` 指向 `http://127.0.0.1:PORT/query`）把
`DsClient` 的真实请求头打印出来，结果如下 —— **这是自相矛盾的组合**：

```
user-agent: DeepSeek/2.5.0 Android/35                        ← 安卓 App
sec-ch-ua: "Chromium";v="136", … "Google Chrome";v="136"     ← 却是 Chrome 浏览器
sec-ch-ua-platform: "macOS"                                  ← 还自称 macOS
sec-fetch-dest: document / mode: navigate / site: none       ← 却是地址栏导航
accept: text/html,application/xhtml+xml,…                    ← 文档请求，不是 XHR
accept-language: en-US,en;q=0.9                              ← 而 client_locale 是 zh_CN
x-client-platform: android
```

真实 Web 客户端（同一轮抓包）是：

```
user-agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) … Chrome/140.0.0.0 Safari/537.36
sec-ch-ua: "Chromium";v="153", "Not_A Brand";v="8"   ← 与真实浏览器版本一致
sec-ch-ua-mobile: ?0 ; sec-ch-ua-platform: "Windows"
accept: */*                    ← XHR
accept-language: zh-CN
referer: https://chat.deepseek.com/
x-client-platform: web ; x-client-version: 2.5.0 ; x-device-id: <uuid> ; x-device-model: (空)
```

也就是说：**我们的 TLS 指纹是 Chrome、client hints 是 Chrome/macOS、UA 却是安卓 App**，
只有「naive 的 HTTP 拟态库」会产出这种组合。

**WAF 兼容性实测**（`cargo run -p ds_core --example identity_probe`，只打无鉴权的
`/client/settings` 与用一次性假凭据打 `/users/login`，**不涉及任何真实账号**）：

| 身份组合 | `/client/settings` | `/users/login`（假凭据） |
|----------|--------------------|--------------------------|
| 安卓 App UA + Chrome136 TLS（当前实现） | 200 ✅ | 200 `biz_code=2` ✅ |
| 桌面 Chrome UA + Chrome136 TLS（全 Web 自洽） | 200 ✅ | 200 `biz_code=2` ✅ |
| 安卓 App UA + OkHttp4.12 TLS（原生 App 自洽） | 200 ✅ | （未测） |

> 注意：这与 2026-09-20 的结论「桌面 Chrome UA 会被 WAF 202 拦截」**不一致**，
> 现在三种身份都能到达应用层。WAF 规则显然变化过，之前的结论已过时。

**因此有两条自洽路线**（都还没做端到端验证，需要干净账号）：

- **A. 全 Web 身份**：`user_agent` 改成与拟态档位一致的 Chrome UA（Chrome136），
  `client_platform = "web"`、`client_os = "web"`，并把 `accept` / `accept-language`
  / `referer` 覆盖成 XHR 形态（参考上面的真实客户端头表）；
- **B. 原生 App 身份**：改用 `Emulation::OkHttp4_12`（默认头只有
  `accept: */*` + `accept-language`），保留安卓 UA 与 `client_os = "android"`，
  不再发 `sec-ch-ua*` / `sec-fetch-*`。

选 A 还是 B，应当用**干净账号**跑一次「登录 → 建会话 → completion」端到端对照后再定，
不要在无法验证的情况下凭手感切换（这正是 2026-09 那轮反复试错的教训）。

### 验证方法与当前状态

单账号最小验证（**逐号、低频**，避免再被禁言）：

1. 浏览器抓包取得该账号真实 `device_id`（数美指纹），并记下同一 profile 的 X-Device-Id；
2. `hif_probe` 先确认风控令牌端点在本机可达（不产生账号流量）；
3. 启动服务 → 账号初始化（登录 / check_device / 健康检查）+ **1 次**真实对话请求；
4. 之后**只做登录**（`account_check`）观察 `is_muted`，观察窗口 ≥ 数小时
   —— 历史对照：同样的低强度流量下，缺失 `x-hif-leim` 时账号在 5–20 分钟内即被禁言。

> 结论待观测窗口结束后回填（见 CHANGELOG / issue #112 评论）。

#### 2026-10-07 实测结果：账号 1 被「临时停用」（结论：无法完成验证）

单账号最小流量实测（`l3366599051@163.com`，全程只有 1 次对话请求）：

| 时间 (UTC) | 事件 | 结果 |
|-----------|------|------|
| 17:14 | 真实浏览器（官方客户端）登录并发 1 条消息 | ✅ 正常，`userIsMuted=false` |
| 17:31 | 服务启动：hif 取令牌 ✅ → login → check_device → 建会话 → 健康检查 completion | ✅ `is_muted=0` |
| 17:33 | 经代理发 **1 次**对话请求（带 `x-hif-leim`，流式返回正常） | ✅ 正常 |
| 17:35 / 17:39 / 17:44 | 登录复查 ×3 | ✅ 均未禁言 |
| **17:52** | 登录复查 | ❌ **`biz_code=10 USER_IS_BANNED`** |

用真实浏览器登录同一账号，官方页面提示：

> **「由于违规次数过多，你的账户已被临时停用」**

**这说明什么、不能说明什么：**

- 该账号在 2026-09 已累计多次违规（禁言 ≈3 天 → ≈8–9 天），本次提示语明确指向
  **「违规次数过多」**，与「阶梯升级处罚」的社区观察一致（1 天 → 3 天 → 8 天 → 永久）；
- 因此本次停用**不能**判定为「`x-hif-leim` 修复无效」——同一账号即使用官方客户端、
  或仅做登录，也可能触发下一级处罚；
- 但同样**不能**判定修复有效。**「是否已可规避封号」目前仍未被证明**，
  v0.5.0 的 release notes 必须如实写明这一点；
- 教训一：**不要用高频登录做监控**。本次 20 分钟内做了 3 次登录复查，
  这不是正常用户行为，本身就可能参与触发；复查应改为 +30min / +2h / +6h 这种低频，
  并优先用浏览器（官方客户端）打开页面观察；
- 教训二：**已被处罚过的账号不能作为验证对象**。要验证「能否规避封号」，
  必须使用**全新、无违规历史**的账号（每个账号独立 `device_id` + 独立 X-Device-Id）。

**下一步的正确实验设计**（待有干净账号时执行）：

1. 账号 A：全新账号，只跑本代理（最小流量），观察 ≥24h；
2. 账号 B：全新账号，只用官方浏览器做同样强度的使用，作为对照组；
3. 两组都低频复查（≤3 次/天），记录 `is_muted` / `mute_until` / 封禁提示语；
4. 只有「A 长期正常且 B 也正常」时，才能说本代理的封号风险与官方客户端接近。



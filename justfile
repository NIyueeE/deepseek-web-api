# Justfile for ai-free-api

set positional-arguments

# Run all checks: type check, lint, format, audit, unused deps
# 前置: cargo install cargo-audit && cargo install cargo-machete && cargo install cargo-outdated
check:
  cargo fmt --all --check
  cargo check --all-targets
  cargo clippy --all-targets -- -D warnings
  # 不加 --deny warnings：wreq / wreq-util 5.x 被上游 yank、lru 0.13 的 unsound 由 wreq 传递引入，
  # 二者都无法在不升级到 wreq 6.0-rc 的前提下消除（详见 .cargo/audit.toml）。
  # 真实漏洞仍然会导致非零退出，与 CI 的 actions-rust-lang/audit 行为一致。
  cargo audit
  # wreq 5.x 全量 yank 会让 cargo-outdated 解析失败，由包装脚本跳过该已知情况
  scripts/check-outdated.sh
  scripts/check-outdated.sh -p ds_core
  cargo machete
  scripts/check-lint-exemptions.sh
  scripts/check-config-drift.sh

# 用假上游校验请求形态（零账号流量）：新建/复用/工具/重复四个用例
# 前置: cargo build（脚本默认用 target/debug/ds-free-api）
verify-payloads:
  bash scripts/risk-experiment/verify-payloads.sh target/debug/ds-free-api

# Build + lint frontend (bun install --frozen-lockfile, bun run typecheck + build + lint)
check-web:
  cd web && bun install --frozen-lockfile && bun run typecheck && bun run lint && bun run check:locales && bun run check:config-parity && bun run build

# 检查 AGENTS.md 的 lint 豁免约定（仅 ds_core/src/accounts/client.rs 允许 #[allow]）
check-lint-exemptions:
  scripts/check-lint-exemptions.sh

# 校验三种语言 locale 键集完全一致
check-locales:
  cd web && bun run check:locales

# 校验 config.example.toml 的字段都在前端出现（避免新增配置在前端漏掉）
check-config-parity:
  cd web && bun run check:config-parity

# 校验 docker/config.example.toml 与根目录 config.example.toml 未漂移
check-config-drift:
  scripts/check-config-drift.sh


# Run unified protocol debug CLI (replaces ds-core-cli / openai-adapter-cli)
# 默认使用 py-e2e-tests/config.toml，可通过 -c <path> 覆盖
adapter-cli *ARGS:
  cargo run --example adapter_cli -- -c py-e2e-tests/config.toml "$@"

# Run openai_adapter/request submodule tests
test-adapter-request *ARGS:
  cargo test openai_adapter::request -- "$@"

# Run openai_adapter/response submodule tests
test-adapter-response *ARGS:
  cargo test openai_adapter::response -- "$@"

# Run HTTP server（自动构建最新前端 -> 启动后端）
serve *ARGS:
  (cd web && bun run build) && cargo run -- "$@"

# Basic: 基础功能测试（两端点）
e2e-basic *ARGS:
  cd py-e2e-tests && uv run python runner.py scenarios/basic "$@"

# Repair: 工具调用损坏修复专项测试
e2e-repair *ARGS:
  cd py-e2e-tests && uv run python runner.py scenarios/repair "$@"

# Stress: 多迭代并发压测（basic + repair 全部场景）
e2e-stress *ARGS:
  cd py-e2e-tests && uv run python stress_runner.py "$@"

# Oversized: 长上下文回退方案测试（expert 分块 + default/vision 文件上传）
e2e-oversized *ARGS:
  cd py-e2e-tests && uv run python test_oversized.py "$@"

# Start server with e2e test config
e2e-serve:
  (cd web && bun run build) && cargo run -- -c py-e2e-tests/config.toml

# Responses: OpenAI Responses API 测试（/v1/responses，流式 + 工具 + previous_response_id）
e2e-responses *ARGS:
  cd py-e2e-tests && uv run python test_responses.py "$@"

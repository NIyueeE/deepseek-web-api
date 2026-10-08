#!/usr/bin/env bash
# 打印低频复查计划（+30min / +2h / +6h / +24h），并给出每条复查命令。
#
# 复查用 `account_check`（只登录一次，读 is_muted / mute_until），
# **不要**用对话请求去「探活」——那正是可能触发风控的行为。
#
# 用法：scripts/risk-experiment/check-plan.sh [-c config.toml] [--email a@b.com]
set -euo pipefail

CONFIG="${DS_CONFIG_PATH:-config.toml}"
EMAIL=""
while [ $# -gt 0 ]; do
  case "$1" in
    -c) CONFIG="$2"; shift 2 ;;
    --email) EMAIL="$2"; shift 2 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

python3 - "$CONFIG" "$EMAIL" <<'PY'
import datetime as dt
import sys

config, email = sys.argv[1], sys.argv[2]
now = dt.datetime.now(dt.timezone.utc).astimezone()
marks = [("+30min", 30), ("+2h", 120), ("+6h", 360), ("+24h", 1440)]

print(f"起点（本地时间 {now.strftime('%Z')}）: {now.strftime('%Y-%m-%d %H:%M')}")
print("复查命令（账号参数可放任意位置）：")
for label, minutes in marks:
    at = now + dt.timedelta(minutes=minutes)
    cmd = f"cargo run --example account_check -- -c {config}"
    if email:
        cmd += f" {email}"
    print(f"  {label:>6}  {at.strftime('%Y-%m-%d %H:%M')}  {cmd}")
print()
print("判读：")
print("  * is_muted=0                  → 仍然正常，继续观察")
print("  * biz_code=5 + mute_until     → 被禁言，记录解禁时间戳并停止使用该账号")
print("  * biz_code=10 USER_IS_BANNED  → 已被停用，登录即被拒（没有解封时间可读）")
PY

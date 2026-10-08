#!/usr/bin/env python3
"""从真实浏览器抓包 JSON 里提取账号的设备指纹，生成可直接粘贴的配置块。

新账号的第一步：用真实浏览器（官方客户端）登录一次，抓包（`cap/capture.js` 会输出
`capture-*.json`）。登录响应里带数美 `device_id`（风控设备指纹），把它写进账号配置；
`X-Device-Id` 由本代理按账号自动派生（`pool::account_x_device_id`），不需要手填。

用法：
    scripts/risk-experiment/extract-device.py <capture.json> <email> [--mobile M] [--area-code 86]

输出：一段 `[[ds_core.accounts]]`（密码留空待填），以及该账号的 X-Device-Id 派生值说明。
退出码：0 = 找到；1 = 没找到（打印抓包里出现过的 API 路径，便于排查）。
"""
import json
import re
import sys


def iter_api_records(doc):
    """兼容两种抓包格式：顶层数组，或 {"records": [...]}"""
    if isinstance(doc, list):
        return doc
    if isinstance(doc, dict):
        for key in ("records", "requests", "log"):
            if isinstance(doc.get(key), list):
                return doc[key]
    return []


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    path, email = sys.argv[1], sys.argv[2]
    mobile, area_code = "", ""
    argv = sys.argv[3:]
    for i, arg in enumerate(argv):
        if arg == "--mobile" and i + 1 < len(argv):
            mobile = argv[i + 1]
        if arg == "--area-code" and i + 1 < len(argv):
            area_code = argv[i + 1]

    with open(path) as f:
        doc = json.load(f)
    records = iter_api_records(doc)
    if not records:
        print(f"❌ {path} 里没有记录数组（期望 list 或 {{'records': [...]}}）")
        return 1

    # 候选按可信度排序：登录请求体 > 其它请求体 > 请求头里的 x-device-id
    candidates = []  # (优先级, 来源描述, 值)
    seen_paths = set()
    for rec in records:
        url = rec.get("url", "")
        m = re.search(r"/api/[^?]*", url)
        if m:
            seen_paths.add(m.group(0))
        is_login = "/users/login" in url
        for field in ("postData", "body", "text"):
            raw = rec.get(field)
            if not isinstance(raw, str) or "device_id" not in raw:
                continue
            for mm in re.finditer(r'"device_id"\s*:\s*"([^"]{8,})"', raw):
                prio = 0 if is_login else 1
                candidates.append((prio, f"{url.split('/api/')[-1]} ({field})", mm.group(1)))
        low = {k.lower(): v for k, v in (rec.get("headers") or {}).items()}
        if low.get("x-device-id"):
            candidates.append((2, f"{url.split('/api/')[-1]} (x-device-id 头)", low["x-device-id"]))

    candidates.sort(key=lambda c: c[0])
    device_id, source = (candidates[0][2], candidates[0][1]) if candidates else ("", "")

    if not device_id:
        print(f"❌ 没在 {path} 里找到 device_id。抓包里出现过的 API 路径：")
        for p in sorted(seen_paths):
            print("   ", p)
        return 1

    print("# 粘贴到 config.toml 的 [ds_core] 段之后（每个账号必须有自己的 device_id）")
    print("[[ds_core.accounts]]")
    print(f'email = "{email}"')
    print(f'mobile = "{mobile}"')
    print(f'area_code = "{area_code}"')
    print('password = ""   # ← 填密码')
    print(f'device_id = "{device_id}"')
    print()
    print(f"# 指纹来源：{source}")
    if re.fullmatch(r"[0-9a-fA-F-]{36}", device_id):
        print("# ⚠️ 取到的是 UUID 形态（X-Device-Id），不是数美 device_id —— 请确认抓包里包含登录请求")
    print("# 提示：X-Device-Id 由上面这个 device_id 自动派生，无需手填；")
    print("#       同一 device_id 不要给多个账号用（会被上游关联）。")
    return 0


if __name__ == "__main__":
    sys.exit(main())

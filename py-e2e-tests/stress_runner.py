#!/usr/bin/env python3
"""压测入口 —— 多迭代并发，跑 basic/ + repair/ 全部场景

安全并发数 = max(1, 账号数 / 2)，stress 默认 = 安全并发数 + 1
"""

import argparse
import json
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
from typing import Any

from config import load_config
from runner import (
    load_scenarios, run_openai, run_anthropic,
    format_duration, print_report,
)
from openai import OpenAI
from anthropic import Anthropic

def main():
    config = load_config()
    safe = config["safe_concurrency"]
    api_key = config["api_key"]
    stress_parallel = safe + 1

    parser = argparse.ArgumentParser(description="端到端压测")
    parser.add_argument("--iterations", type=int, default=3, help="每场景迭代数 (默认: 3)")
    parser.add_argument("--parallel", type=int, default=stress_parallel, help=f"并行数 (默认: {stress_parallel})")
    parser.add_argument("--models", type=str, nargs="*", default=None, help="模型过滤")
    parser.add_argument("--filter", type=str, nargs="*", default=None, help="场景名称关键字过滤（多个用空格分隔）")
    parser.add_argument("--report", type=str, default=None, help="输出 JSON 报告路径")
    parser.add_argument("--show-output", action="store_true", help="显示模型输出内容")
    args = parser.parse_args()

    # 加载全部场景
    basic_oai = load_scenarios("scenarios/basic", "openai", args.filter)
    basic_anth = load_scenarios("scenarios/basic", "anthropic", args.filter)
    repair_sc = load_scenarios("scenarios/repair", None, args.filter)
    all_scenarios = basic_oai + basic_anth + repair_sc

    # 上游已下线 expert / vision（/api/v0/client/settings 中 enabled=false），
    # 默认只压测 default，避免产生大量假失败；需要时用 --models 显式指定。
    models = args.models or ["deepseek-default"]

    port = config["port"]
    # 同 runner.py：anthropic>=1.5 使用 httpx2，不能传 http_client
    oai_client = OpenAI(base_url=f"http://127.0.0.1:{port}/v1", api_key=api_key, timeout=120)
    anth_client = Anthropic(
        base_url=f"http://127.0.0.1:{port}/anthropic", api_key=api_key,
        default_headers={"Authorization": f"Bearer {api_key}"},
        timeout=120,
    )

    total_scenarios = len(all_scenarios)
    total_requests = total_scenarios * len(models) * args.iterations

    print(f"\n端到端压测")
    print(f"  场景: {total_scenarios} 个 (basic + repair)")
    print(f"  模型: {', '.join(models)}")
    print(f"  迭代: {args.iterations} 次/场景/模型")
    print(f"  并行: {args.parallel}")
    print(f"  总计: {total_requests} 次请求\n")

    tasks: list[tuple[str, str, dict, int]] = []
    for model in models:
        for sc in all_scenarios:
            for i in range(args.iterations):
                tasks.append((sc["endpoint"], model, sc, i))

    all_results: list[dict[str, Any]] = [None] * len(tasks)  # type: ignore[list-item]

    start_total = time.time()
    with ThreadPoolExecutor(max_workers=args.parallel) as executor:
        def run_task(endpoint: str, model: str, sc: dict, _idx: int) -> tuple[int, dict]:
            if endpoint == "openai":
                result = run_openai(oai_client, sc, model)
            else:
                result = run_anthropic(anth_client, sc, model)
            return (_idx, result)

        ep_label = {"openai": "OAI", "anthropic": "ANT"}
        task_labels: dict[int, str] = {}
        for i, (ep, model, sc, it) in enumerate(tasks):
            task_labels[i] = f"{ep_label.get(ep, '?')} | {sc['name']} | {model} | iter-{it + 1}"

        future_map = {}
        for i, (ep, model, sc, _) in enumerate(tasks):
            future = executor.submit(run_task, ep, model, sc, i)
            future_map[future] = i

        done = 0
        passed = 0
        for future in as_completed(future_map):
            idx = future_map[future]
            _, result = future.result()
            all_results[idx] = result
            done += 1
            if result["passed"]:
                passed += 1
            label = task_labels[idx]
            status = "✓" if result["passed"] else "✗"
            err = f" | {result['error'][:60]}" if result["error"] else ""
            print(f"  [{done}/{total_requests}] {status} | {label} | {result['duration']:.1f}s{err}")
            if args.show_output:
                from runner import _print_output
                _print_output(result)

    total_duration = time.time() - start_total
    print(f"\n  总耗时: {format_duration(total_duration)}")

    report = print_report(all_results, "端到端压测报告", args.parallel)
    report["total_duration"] = round(total_duration, 1)

    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            json.dump({
                "suite": "stress",
                "started_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                "config": {
                    "iterations": args.iterations,
                    "parallel": args.parallel,
                    "models": models,
                    "accounts": config["accounts"],
                },
                "summary": report,
                "results": all_results,
            }, f, ensure_ascii=False, indent=2)
        print(f"  报告已输出: {args.report}")

    sys.exit(0 if report["failed"] == 0 else 1)


if __name__ == "__main__":
    main()

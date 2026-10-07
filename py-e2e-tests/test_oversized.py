#!/usr/bin/env python3
"""长上下文回退方案测试 —— 验证 oversized 检测 + 分块逻辑

构造超阈值长文本，测试 default 的「历史拆分 + 文件上传」回退路径；
expert 的「分块 completion」路径仍保留（需上游重新开放 expert，当前 `enabled=false`，
直接跑 expert 只会拿到空回复 —— 那属于上游状态，不是本代理的缺陷）。

用法：
  uv run python test_oversized.py                       # 默认只测 default
  uv run python test_oversized.py --model deepseek-expert   # 上游已下线，仅作历史参考
  uv run python test_oversized.py --show-output
"""

import argparse
import json
import sys
import time
from datetime import datetime
from pathlib import Path

from openai import OpenAI

from config import load_config


def make_long_prompt(target_chars: int) -> str:
    """构造一个刚好超 threshold 的长文本 prompt
    """
    base = "deepseek"
    repeat = target_chars // len(base) + 1
    return base * repeat


def run_oversized(client: OpenAI, model: str, threshold: int) -> dict:
    """执行一次 oversized 测试"""
    prompt = make_long_prompt(threshold + 1)

    start = time.time()
    result: dict = {
        "model": model,
        "threshold": threshold,
        "passed": False,
        "duration": 0.0,
        "output_len": 0,
        "output_preview": "",
        "error": None,
    }

    try:
        response = client.chat.completions.create(
            model=model,
            messages=[
                {"role": "system", "content": "你是一个有帮助的助手。无论如何, 请你只回复一句`Hello, world!`即可"},
                {"role": "user", "content": prompt},
            ],
            stream=True,
            max_tokens=100,
        )

        content_parts: list[str] = []
        for chunk in response:
            if chunk.choices and chunk.choices[0].delta.content:
                content_parts.append(chunk.choices[0].delta.content)

        result["duration"] = time.time() - start
        result["output_len"] = len("".join(content_parts))
        result["output_preview"] = "".join(content_parts)[:200]
        result["passed"] = len(content_parts) > 0
        if not result["passed"]:
            result["error"] = "返回内容为空"

    except Exception as e:
        result["duration"] = time.time() - start
        result["error"] = str(e)

    return result


def _print_output(result: dict) -> None:
    """打印模型输出内容（与 runner.py 对称）"""
    content = (result.get("output_preview") or "")[:300].replace("\n", "\\n")
    if content:
        print(f"    ├ 回复: {content}")
    if result.get("error"):
        print(f"    └ 错误: {result['error']}")


def format_duration(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:.1f}s"
    return f"{seconds / 60:.1f}m"


def print_report(results: list[dict]) -> dict:
    """打印汇总报告（与 runner.py 对称）"""
    total = len(results)
    passed = sum(1 for r in results if r["passed"])
    duration = sum(r["duration"] for r in results)

    print(f"\n{'=' * 60}")
    print(f"  长上下文回退方案测试")
    print(f"  时间: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"{'=' * 60}")
    print(f"  总计: {total}  |  通过: {passed}  |  失败: {total - passed}  |  总耗时: {format_duration(duration)}")

    for r in sorted(results, key=lambda x: x["model"]):
        status = "✓" if r["passed"] else "✗"
        fallback = "分块 completion" if "expert" in r["model"] else "文件上传"
        err = f" | {r['error'][:60]}" if r["error"] else ""
        print(f"    {status} {r['model']} | {fallback} | threshold={r['threshold']} | {r['duration']:6.2f}s | {r['output_len']} chars{err}")

    if total - passed > 0:
        print(f"\n  {'─' * 48}")
        print(f"  失败详情:")
        for r in results:
            if not r["passed"]:
                print(f"  {r['model']}: {r['error']}")

    print(f"{'=' * 60}\n")
    return {"total": total, "passed": passed, "failed": total - passed, "duration": duration}


def main():
    parser = argparse.ArgumentParser(description="长上下文回退方案测试")
    parser.add_argument("--model", type=str, default=None, help="只测试指定模型（默认 deepseek-default；expert 上游已下线）")
    parser.add_argument("--show-output", action="store_true", help="显示模型输出内容")
    parser.add_argument("--report", type=str, default=None, help="输出 JSON 报告路径")
    args = parser.parse_args()

    config = load_config()
    client = OpenAI(
        base_url=f"http://127.0.0.1:{config['port']}/v1",
        api_key=config["api_key"],
    )

    # 从 config 动态构建阈值表
    threshold_map = {
        f"deepseek-{t}": (limit * 75 // 100)
        for t, limit in zip(config["model_types"], config["input_character_limits"])
    }

    models = [args.model] if args.model else config["models"]

    suite_name = "长上下文回退方案测试"
    print(f"\n{suite_name}")
    print(f"  模型: {', '.join(models)}")

    # 先测 expert（分块），再测 default/vision（文件上传）
    sorted_models = sorted(models, key=lambda m: (0 if "expert" in m else 1, m))
    fallback_labels = {"expert": "分块 completion", "default": "文件上传", "vision": "文件上传"}

    results: list[dict] = []
    done = 0
    for model in sorted_models:
        threshold = threshold_map.get(model, 122_880)
        fb = next((v for k, v in fallback_labels.items() if k in model), "?")

        r = run_oversized(client, model, threshold)
        results.append(r)
        done += 1

        status = "✓" if r["passed"] else "✗"
        err = f" | {r['error'][:60]}" if r["error"] else ""
        print(f"  [{done}/{len(sorted_models)}] {status} | {fb} | {model} | {r['duration']:.1f}s | {r['output_len']} chars{err}")
        if args.show_output:
            _print_output(r)

    report = print_report(results)

    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            json.dump({
                "suite": suite_name,
                "started_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                "summary": report,
                "results": results,
            }, f, ensure_ascii=False, indent=2)
        print(f"  报告已输出: {args.report}")

    sys.exit(0 if report["failed"] == 0 else 1)


if __name__ == "__main__":
    main()

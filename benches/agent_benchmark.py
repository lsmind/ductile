#!/usr/bin/env python3
"""
Ductile Agent Benchmark: ollama 本地模型 vs 云端 LLM
对比 agent 任务成功率——不经过 Ductile engine，直接测 API 质量。

任务集（5 个 agent 典型任务）：
1. 信息提取（从搜索结果提取关键信息）
2. 任务分解（把大任务拆成子任务 + cost）
3. 结构化总结（把散乱信息整理成结构化报告）
4. 条件判断（根据给定规则判断是否满足条件）
5. 代码生成（生成简单的 Python 函数）
"""

import json, time, urllib.request, os, sys

# ── 配置 ──
OLLAMA_URL = "http://127.0.0.1:11434/api/chat"
NEWAPI_URL = "http://127.0.0.1:3000/v1/chat/completions"

# 从 .bashrc 读 key
def get_key():
    import subprocess
    r = subprocess.run(["bash", "-c", "grep GLM_API_KEY ~/.bashrc | head -1 | cut -d'\"' -f2"], capture_output=True, text=True)
    return r.stdout.strip()

API_KEY = get_key()

# ── 测试模型 ──
MODELS = {
    "qwen3.6 (23B, local)": {"type": "ollama", "model": "qwen3.6:latest"},
    "rwkv7 (2.9B, local)":  {"type": "ollama", "model": "heredos/rwkv7:2.9b"},
    "glm-5 (cloud)":        {"type": "newapi", "model": "glm-5"},
}

# ── 5 个 Agent 任务 ──
TASKS = [
    {
        "name": "信息提取",
        "system": "你是一个信息提取助手。从给定文本中提取关键信息，输出 JSON。",
        "prompt": """从以下搜索结果中提取每条结果的标题和URL，输出 JSON 数组。

搜索结果：
1. Transformer模型详解 - https://example.com/transformer
2. 注意力机制原理解析 - https://example.com/attention
3. BERT与GPT对比分析 - https://example.com/bert-vs-gpt

只输出 JSON，不要其他文字。格式：[{"title":"...","url":"..."}]""",
        "validate": lambda r: r.strip().startswith("[") and "title" in r and "url" in r,
    },
    {
        "name": "任务分解",
        "system": "你是一个项目分解助手。把大任务拆成子任务并估算成本。",
        "prompt": """把"搭建一个个人博客网站"分解为5个子任务。
每个子任务一行，格式：子任务名称 | latency=毫秒数 risk=0-1 tokens=数字 money=美元

示例格式：
购买域名 | latency=60000 risk=0.1 tokens=0 money=10
选择服务器 | latency=300000 risk=0.2 tokens=0 money=50""",
        "validate": lambda r: "|" in r and "latency=" in r and r.count("\n") >= 4,
    },
    {
        "name": "结构化总结",
        "system": "你是一个技术分析师。把散乱信息整理成结构化报告。",
        "prompt": """把以下信息整理成一份结构化技术报告，包含：概述、核心技术、应用场景、总结。

---
Rust 是一种系统编程语言，追求性能和安全。
它通过所有权机制在编译期消除数据竞争。
Actix 是 Rust 的高性能 Web 框架。
Tokio 是异步运行时。
Rust 被 Discord、Dropbox、Cloudflare 使用。
内存安全无需 GC，零成本抽象。""",
        "validate": lambda r: len(r) > 100 and ("概述" in r or "总结" in r or "应用" in r),
    },
    {
        "name": "条件判断",
        "system": "你是一个规则引擎。根据给定条件判断是否通过。",
        "prompt": """判断以下场景是否满足条件，输出 PASS 或 FAIL 并说明理由。

条件：视频时长必须在 5-30 秒之间，分辨率不低于 720p，帧率不低于 24fps。
数据：时长=8.5秒，分辨率=1080p，帧率=30fps。

输出格式：PASS/FAIL - 理由""",
        "validate": lambda r: "PASS" in r or "FAIL" in r,
    },
    {
        "name": "代码生成",
        "system": "你是一个 Python 程序员。生成可运行的代码。",
        "prompt": """写一个 Python 函数 calculate_cost(latency_ms, risk, tokens, money)，
计算 agent 任务的总成本。
公式：cost = latency_ms * 0.001 + risk * 100 + tokens * 0.001 + money * 10

只输出代码，不要解释。""",
        "validate": lambda r: "def calculate_cost" in r and "return" in r,
    },
]

# ── API 调用 ──
def call_ollama(model, system, prompt, timeout=60):
    data = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "stream": False,
        "options": {"temperature": 0.3}
    }).encode()
    req = urllib.request.Request(OLLAMA_URL, data=data, headers={"Content-Type": "application/json"})
    start = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            r = json.loads(resp.read())
            elapsed = time.time() - start
            return r["message"]["content"], elapsed, None
    except Exception as e:
        elapsed = time.time() - start
        return "", elapsed, str(e)

def call_newapi(model, system, prompt, timeout=60):
    data = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "temperature": 0.3,
    }).encode()
    req = urllib.request.Request(NEWAPI_URL, data=data, headers={
        "Content-Type": "application/json",
        "Authorization": f"Bearer {API_KEY}"
    })
    start = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            r = json.loads(resp.read())
            elapsed = time.time() - start
            return r["choices"][0]["message"]["content"], elapsed, None
    except Exception as e:
        elapsed = time.time() - start
        return "", elapsed, str(e)

def call_model(model_cfg, system, prompt, timeout=60):
    if model_cfg["type"] == "ollama":
        return call_ollama(model_cfg["model"], system, prompt, timeout)
    else:
        return call_newapi(model_cfg["model"], system, prompt, timeout)

# ── 运行测试 ──
print("=" * 70)
print("Ductile Agent Benchmark: 本地 ollama vs 云端 LLM")
print("=" * 70)

results = {}  # {model_name: {task_name: {success, latency, error}}}

for model_name, model_cfg in MODELS.items():
    print(f"\n{'─'*50}")
    print(f"模型: {model_name}")
    print(f"{'─'*50}")
    results[model_name] = {}

    for task in TASKS:
        sys.stdout.write(f"  {task['name']}... ")
        sys.stdout.flush()

        response, elapsed, error = call_model(
            model_cfg, task["system"], task["prompt"], timeout=120
        )

        if error:
            print(f"❌ ERROR ({elapsed:.1f}s): {error[:60]}")
            results[model_name][task["name"]] = {
                "success": False, "latency": elapsed, "error": error[:80],
                "response_preview": ""
            }
        else:
            passed = task["validate"](response)
            symbol = "✅" if passed else "⚠️ "
            print(f"{symbol} {'PASS' if passed else 'FAIL'} ({elapsed:.1f}s, {len(response)} chars)")
            results[model_name][task["name"]] = {
                "success": passed, "latency": elapsed, "error": None,
                "response_preview": response[:120].replace("\n", " ")
            }

# ── 汇总 ──
print(f"\n{'='*70}")
print("汇总")
print(f"{'='*70}")
print(f"{'模型':<25} {'任务':<12} {'结果':<6} {'延迟':>8} {'字符数':>8}")
print(f"{'─'*65}")

for model_name in MODELS:
    successes = sum(1 for t in results[model_name].values() if t["success"])
    total = len(TASKS)
    avg_latency = sum(t["latency"] for t in results[model_name].values()) / total
    print(f"\n{model_name}  ({successes}/{total} 成功, 平均 {avg_latency:.1f}s)")
    for task_name, r in results[model_name].items():
        status = "✅" if r["success"] else "❌"
        chars = len(r.get("response_preview", ""))
        print(f"  {status} {task_name:<12} {r['latency']:>7.1f}s")

# 成功率对比
print(f"\n{'='*70}")
print("成功率对比")
print(f"{'='*70}")
for model_name in MODELS:
    successes = sum(1 for t in results[model_name].values() if t["success"])
    total = len(TASKS)
    rate = successes / total * 100
    bar = "█" * int(rate / 10) + "░" * (10 - int(rate / 10))
    print(f"  {model_name:<25} {bar} {rate:.0f}% ({successes}/{total})")

print(f"\n{'='*70}")
print("结论")
print(f"{'='*70}")
local_results = {}
cloud_results = {}
for model_name in MODELS:
    successes = sum(1 for t in results[model_name].values() if t["success"])
    rate = successes / len(TASKS) * 100
    if "local" in model_name:
        local_results[model_name] = rate
    else:
        cloud_results[model_name] = rate

if local_results and cloud_results:
    avg_local = sum(local_results.values()) / len(local_results)
    avg_cloud = sum(cloud_results.values()) / len(cloud_results)
    print(f"  本地模型平均成功率: {avg_local:.0f}%")
    print(f"  云端模型平均成功率: {avg_cloud:.0f}%")
    diff = avg_cloud - avg_local
    if diff > 0:
        print(f"  云端比本地高 {diff:.0f} 个百分点")
    elif diff < 0:
        print(f"  本地比云端高 {-diff:.0f} 个百分点")
    else:
        print(f"  两者持平")

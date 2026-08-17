#!/usr/bin/env python3
"""
Ductile DSL Authoring Benchmark:
给模型喂 SPEC.md，让它自己写 .pipeline 文件，然后跑 pipeline-dsl check 验证合法性。

测试维度：
  1. 语法正确性 —— pipeline-dsl check 是否通过
  2. 语义合理性 —— proc 之间的 @引用、check、cost 是否有意义
  3. 通过率    —— N 个任务中多少能产出可用的 pipeline
"""

import json, time, urllib.request, subprocess, os, sys, tempfile, re

# ── 配置 ──
OLLAMA_URL = "http://127.0.0.1:11434/api/chat"
NEWAPI_URL = "http://127.0.0.1:3000/v1/chat/completions"
SPEC_PATH = os.path.expanduser("~/projects/pipeline-dsl/SPEC.md")
DSL_BIN = os.path.expanduser("~/projects/pipeline-dsl/dist-newstyle/build/x86_64-linux/ghc-9.4.8/pipeline-dsl-0.1.0.0/x/pipeline-dsl/build/pipeline-dsl/pipeline-dsl")

def get_key():
    import subprocess
    r = subprocess.run(["bash", "-c", "grep GLM_API_KEY ~/.bashrc | head -1 | cut -d'\"' -f2"], capture_output=True, text=True)
    return r.stdout.strip()

API_KEY = get_key()

with open(SPEC_PATH, "r") as f:
    SPEC_TEXT = f.read()

# ── 测试模型 ──
MODELS = {
    "qwen3.6 (23B)":   {"type": "ollama", "model": "qwen3.6:latest"},
    "rwkv7 (2.9B)":    {"type": "ollama", "model": "heredos/rwkv7:2.9b"},
    "glm-5 (cloud)":   {"type": "newapi", "model": "glm-5"},
}

# ── 5 个 pipeline 编写任务 ──
# 从简单到复杂，覆盖不同 DSL 特性
TASKS = [
    {
        "id": "simple_search",
        "prompt": """根据上面的 SPEC，写一个 .pipeline 文件：
任务：搜索某个话题的最新新闻，然后写一份简报到文件。

要求：
- 至少 2 个 proc
- 用到 .check
- 用到 .cost
- 最后 .deliver

只输出 .pipeline 文件内容，不要其他解释。以 Pipeline( 开头。""",
    },
    {
        "id": "multi_path",
        "prompt": """根据上面的 SPEC，写一个 .pipeline 文件：
任务：获取天气信息，有两条路径——API 调用（快但可能失败）和网页爬取（慢但稳定）。
API 路径 risk=0.3，爬取路径 risk=0.05。

要求：
- 同一个 proc 里要有 2 个 impl（多路径）
- 用到 @引用传递数据
- 用到 .check 做结果验证

只输出 .pipeline 文件内容。以 Pipeline( 开头。""",
    },
    {
        "id": "decompose_foreach",
        "prompt": """根据上面的 SPEC，写一个 .pipeline 文件：
任务：深度研究流程——先用 LLM 把一个大话题分解成 5 个子话题，
然后对每个子话题搜索，最后汇总写成报告。

要求：
- 用到 .foreach
- 至少 4 个 proc
- 分解 proc 用 llm，搜索 proc 用 web_search 或 mcp_search
- 最后一个 proc 写文件

只输出 .pipeline 文件内容。以 Pipeline( 开头。""",
    },
    {
        "id": "check_comparison",
        "prompt": """根据上面的 SPEC，写一个 .pipeline 文件：
任务：生成视频，然后检查时长是否在合理范围内。

要求：
- 第一个 proc 用 run() 执行外部程序
- 外部程序输出 ##DSL_RESULT 块回传 duration 字段
- 第二个 proc 的 check 用数值比较（duration < 30.0）
- 用到 @video_gen.duration 字段引用

只输出 .pipeline 文件内容。以 Pipeline( 开头。""",
    },
    {
        "id": "complex_workflow",
        "prompt": """根据上面的 SPEC，写一个 .pipeline 文件：
任务：每日工作复盘自动化。

流程：
1. 读取今日工作笔记文件
2. 用 LLM 分析：做了什么、遇到什么问题、明天计划
3. 检查分析结果是否包含"明日"或"计划"关键词
4. 如果分析失败，用模板拼接作为 fallback（多路径）
5. 写入 Obsidian vault

要求：
- 至少 4 个 proc
- 至少一个 proc 有 2 条 impl 路径
- 用到多个 .check
- 最后 .deliver

只输出 .pipeline 文件内容。以 Pipeline( 开头。""",
    },
]

# ── API 调用 ──
def call_ollama(model, system, prompt, timeout=120):
    data = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "stream": False,
        "options": {"temperature": 0.4}
    }).encode()
    req = urllib.request.Request(OLLAMA_URL, data=data, headers={"Content-Type": "application/json"})
    start = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            r = json.loads(resp.read())
            elapsed = time.time() - start
            return r["message"]["content"], elapsed, None
    except Exception as e:
        return "", time.time() - start, str(e)

def call_newapi(model, system, prompt, timeout=60):
    data = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "temperature": 0.4,
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
        return "", time.time() - start, str(e)

def call_model(cfg, system, prompt, timeout=120):
    if cfg["type"] == "ollama":
        return call_ollama(cfg["model"], system, prompt, timeout)
    else:
        return call_newapi(cfg["model"], system, prompt, timeout)

# ── 提取 pipeline 文本 ──
def extract_pipeline(text):
    """从模型回复中提取 .pipeline 文件内容"""
    # 尝试找 ``` 代码块
    m = re.search(r'```\w*\n(.*?)```', text, re.DOTALL)
    if m:
        return m.group(1).strip()
    # 尝试找 Pipeline( 开头的部分
    lines = text.strip().split("\n")
    start = None
    for i, line in enumerate(lines):
        if line.strip().startswith("Pipeline(") or line.strip().startswith("Pipeline ("):
            start = i
            break
    if start is not None:
        return "\n".join(lines[start:])
    return text.strip()

# ── 验证 pipeline ──
def check_pipeline(pipeline_text, task_id):
    """写入临时文件并跑 pipeline-dsl check"""
    tmpdir = tempfile.mkdtemp()
    filepath = os.path.join(tmpdir, f"{task_id}.pipeline")
    with open(filepath, "w") as f:
        f.write(pipeline_text)
    try:
        r = subprocess.run(
            [DSL_BIN, "check", filepath],
            capture_output=True, text=True, timeout=30
        )
        return r.returncode == 0, r.stdout.strip() + r.stderr.strip()
    except Exception as e:
        return False, str(e)

# ── 运行测试 ──
SYSTEM_PROMPT = f"""你是 Ductile（铸渠）Agent Pipeline DSL 的专家。
以下是你必须遵循的 DSL 规范：

{SPEC_TEXT}

严格遵守上述语法。"""

print("=" * 72)
print("Ductile DSL Authoring Benchmark")
print("测试模型阅读 SPEC.md 后编写 .pipeline 文件的能力")
print("=" * 72)

results = {}  # {model: {task_id: {check_passed, error, latency, chars}}}

for model_name, model_cfg in MODELS.items():
    print(f"\n{'━'*60}")
    print(f"模型: {model_name}")
    print(f"{'━'*60}")
    results[model_name] = {}

    for task in TASKS:
        sys.stdout.write(f"  [{task['id']}] 生成中... ")
        sys.stdout.flush()

        response, elapsed, error = call_model(
            model_cfg, SYSTEM_PROMPT, task["prompt"], timeout=180
        )

        if error:
            print(f"❌ API ERROR ({elapsed:.0f}s): {error[:50]}")
            results[model_name][task["id"]] = {
                "check_passed": False, "error": f"API: {error[:60]}",
                "latency": elapsed, "chars": 0, "response": ""
            }
            continue

        # 提取 pipeline 文本
        pipeline_text = extract_pipeline(response)

        # 验证
        passed, msg = check_pipeline(pipeline_text, task["id"])

        if passed:
            print(f"✅ PASS ({elapsed:.0f}s, {len(pipeline_text)} chars)")
        else:
            # 取错误第一行
            err_line = msg.split("\n")[0] if msg else "unknown"
            print(f"❌ FAIL ({elapsed:.0f}s, {len(pipeline_text)} chars): {err_line[:60]}")

        results[model_name][task["id"]] = {
            "check_passed": passed,
            "error": msg[:200] if not passed else "",
            "latency": elapsed,
            "chars": len(pipeline_text),
            "response": pipeline_text[:300]
        }

# ── 汇总 ──
print(f"\n{'='*72}")
print("汇总报告")
print(f"{'='*72}\n")

print(f"{'任务':<20}", end="")
for mn in MODELS:
    print(f" {mn:<16}", end="")
print()
print(f"{'─'*68}")

for task in TASKS:
    print(f"  {task['id']:<18}", end="")
    for mn in MODELS:
        r = results[mn][task["id"]]
        sym = "✅" if r["check_passed"] else "❌"
        print(f" {sym} {r['latency']:>3.0f}s{'':<7}", end="")
    print()

print(f"\n{'─'*68}")
print(f"  {'成功率':<18}", end="")
for mn in MODELS:
    s = sum(1 for t in results[mn].values() if t["check_passed"])
    total = len(TASKS)
    rate = s / total * 100
    print(f" {s}/{total} ({rate:>3.0f}%){'':<4}", end="")
print()

print(f"  {'平均延迟':<18}", end="")
for mn in MODELS:
    avg = sum(t["latency"] for t in results[mn].values()) / len(TASKS)
    print(f" {avg:>5.0f}s{'':<8}", end="")
print()

print(f"  {'平均字符数':<18}", end="")
for mn in MODELS:
    avg = sum(t["chars"] for t in results[mn].values()) / len(TASKS)
    print(f" {avg:>5.0f}{'':<9}", end="")
print()

# ── 失败原因分析 ──
print(f"\n{'='*72}")
print("失败原因分析")
print(f"{'='*72}")
for mn in MODELS:
    fails = [(tid, results[mn][tid]) for tid in results[mn] if not results[mn][tid]["check_passed"]]
    if fails:
        print(f"\n  {mn}:")
        for tid, r in fails:
            err = r["error"].split("\n")[0][:80] if r["error"] else "?"
            print(f"    ❌ {tid}: {err}")

# ── 样例输出 ──
print(f"\n{'='*72}")
print("每模型第一个成功任务样例（前 20 行）")
print(f"{'='*72}")
for mn in MODELS:
    for tid in results[mn]:
        if results[mn][tid]["check_passed"]:
            print(f"\n--- {mn} / {tid} ---")
            lines = results[mn][tid]["response"].split("\n")[:20]
            for l in lines:
                print(f"  {l}")
            break
    else:
        print(f"\n--- {mn}: 无成功任务 ---")

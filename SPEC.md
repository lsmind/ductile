# Ductile DSL — 规格文档（v0.19）

> 面向 AI agent / LLM 调用者与人类维护者。读完应能独立完成安装、管线编写、执行、调试、调优。
> 本文只描述**当前状态**；历史沿革见 git log，不在此堆叠。

---

## 0. 安装与快速开始

```bash
pip install ductile          # PyPI wheel（cp311 manylinux，maturin 构建）
ductile --help
```

源码编译（开发态）：

```bash
git clone https://github.com/lsmind/ductile.git
cd ductile && cargo build --release   # 二进制 target/release/ductile
```

最小管线：

```
Pipeline("hello", "我的第一条管线")
  .proc("greet", run("echo hello ductile"))
  .proc("save", write(to="/tmp/hello.txt", content=@greet))
  .proc("deliver")
    .deliver(@save)
```

```bash
ductile check hello.pipeline   # 先检查（好习惯：错误在执行前被拦下）
ductile run hello.pipeline     # 再执行
```

新手向导见 [README.md](README.md)（5 分钟上手 + 八级功能阶梯）。

---

## 1. 管线文件（.pipeline）

### 1.1 文件格式

纯文本，扩展名 `.pipeline`；`//` 注释；`#!` 首行可作 shebang 直跑（`#!/…/ductile run` + `chmod +x`）。

### 1.2 完整语法

```
Pipeline("name", "desc", cwd="...", env=["K=V", ...])
  .proc("proc_name")
    .desc("说明")
    .plan(
      impl_a -> verb(args)
        .tags(#tag1, #tag2)
        .desc("impl 说明")
        .retry(n=3)
        .when(mode == "deep")     // 内联 when（impl 级）
        .disabled
        .stub,
      impl_b -> verb(args)
    )
    .when(@upstream.field OP value)  // 块级 when（下推到全部无内联 when 的 impl）
    .needs(@upstream)             // 上下文数据流（喂 LLM 合成 prompt）
    .trust(@upstream)             // shell 注入信任声明（run/sh 体内 @ref 必须）
    .constraint(field)            // 链级约束继承（裸字段名）
    .contract(outputs="a,b", invariants="@self.score >= 80")
    .foreach(source=@upstream, var=item)
    .pick(by=history)             // 排序通道声明
  .proc("deliver")                // 哨兵 proc 不执行，只标记终端产出
    .deliver(@save)
```

**单 impl 内联糖**：`.proc("name", verb(args))` ≡ `.plan(verb -> verb(args))`，impl 名自动取动词名，尾部修饰符照常。箭头 `name -> body` 只在多路选择 `.plan(a -> …, b -> …)` 里出现。未手写 `.tags` 时自动取动词名（#run/#write/#script…），语义域标记（#git/#gate）手写叠加。未知动词不脱糖，fail-closed。

**管线级 cwd/env**：`cwd=` 锚定 run/spawn 子进程与 fs 动词的相对路径（值内可写 `$VAR`/`$(...)`/`~`，bash 规范化）；`env=["K=V"]` 注入全部子进程，PATH/HOME/USER 永不覆盖。未声明 = 旧行为。

### 1.3 修饰符语义总表

| 修饰符 | 级别 | 语义 | 硬错误形态 |
|---|---|---|---|
| `.desc(s)` | 任意 | 说明文字 | — |
| `.tags(#a)` | impl | 检索/同构线索 | — |
| `.retry(n=)` | impl | 失败重试 n+1 次，退避 2s→4s→8s | — |
| `.when(cond)` | impl / proc | 门禁/路由，引擎内求值 fail-closed | 空条件/括号不平衡/裸 `@proc`（须 `.field`） |
| `.disabled` / `.stub` | impl | 禁用 / 桩 | — |
| `.needs(@a, @b)` | proc | 数据流：上游产出并入 LLM 合成 prompt 上下文；建排序边 | 裸词（须 `@name`） |
| `.trust(@a)` | proc | 注入信任：run()/sh() 体内引用 `@ref` 必须点名 | 裸词/空 |
| `.constraint(f)` | proc | 链级约束：本 proc 的字段值注入全部下游合成 prompt | `@x`（带 @）/空/带括号 |
| `.contract(outputs=, invariants=)` | proc | 节点契约卡：执行后确定性校验（L1 字段存在 + L2 谓词） | — |
| `.foreach(source=@x, var=item)` | proc | 逐条展开（source 进排序边） | source 不存在 |
| `.pick(by=…)` | proc | 排序通道，默认 `history` | — |
| `.deliver(@x)` | proc | 终端产出标记 | 自指/幽灵引用/空（parse 期拒） |

### 1.4 变量替换（resolve_vars，执行前统一解析）

| 语法 | 替换为 | 未命中时 |
|---|---|---|
| `{topic}` | run 的 topic 参数 | 原样保留 |
| `{hash(topic)}` | topic 的 djb2 短哈希 | 原样保留 |
| `{var}` | foreach 当前 item（或 results 内任意 proc 值） | 原样保留（JSON 字面花括号安全） |
| `{var.field}` | 同上，字段通道 | 原样保留 |
| `@proc` | 上游 proc 完整输出文本 | 原样保留 |
| `@proc.field` | 上游 DSL_RESULT 字段值 | `<no field:proc.field>` |

### 1.5 内置动词全表（step_registry，21 个）

| 动词 | 形态 | 说明 |
|---|---|---|
| `run` / `sh` | `run("cmd", timeout=300, env="K=V")` | shell 执行；timeout=0 不限；env 可重复 |
| `write` | `write(to="path", content=@ref)` | Rust 侧解析 @ref 落盘，不过 shell |
| `read` / `read_file` | `read(from="path")` | 读文件 |
| `llm` | `llm(agent)` 或 `llm(agent, prompt="…", schema="…", tier="…")` | OpenAI 兼容；见 §5 |
| `script` | `script(name, k=v, …)` | 脚本契约调用，见 §7 |
| `merge` | `merge(@a, @b, dedup)` | 合并结果（可选去重） |
| `search` / `mcp_search` / `web_search` | `search(query="…")` | 检索动词（桥接） |
| `spawn` | `spawn(name="h", cmd="…")` | 后台启动返回 pid，自立进程组 |
| `procs` | `procs(name="h")` | 列句柄：name pid age_s alive cmd |
| `kill` | `kill(name="h")` | SIGKILL 整进程组（防孤儿）；接受裸 pid |
| `wait` | `wait(name="h", timeout=60)` | 阻塞等句柄退出；僵尸感知 |
| `exists` | `exists("path")` | → "true"/"false" |
| `stat` | `stat("path")` | → "kind size_bytes mtime_unix" |
| `ls` | `ls("dir")` | → 每行一个条目，排序 |
| `cp` | `cp(from="src", to="dst")` | 文件直拷（目录树用 shell cp -r） |
| `mkdir` | `mkdir("path")` | create_dir_all |
| `rm` | `rm("path")` | 递归删除；拒 / 与 $HOME 根（编译期硬保护） |
| `disk` | `disk("/tmp")` | → "avail_gb total_gb" |

未知动词 fail-closed（`<noop>` 假成功已删除）。

### 1.6 @ref 进 shell 的铁律（.trust 闸）

`run()`/`sh()` 命令体里引用 `@proc` / `@proc.field`，必须在该 proc 声明 `.trust(@proc)`：

- **parser 静态闸**：check 期扫描 run/sh 体，命中真实 proc 名且未点名 → ParseError（带行号）
- **executor 运行时兜底**：resolve 后残留的 `@name`（动态拼接形态）同判
- 豁免：`@self`、`@localhost`（主机名/邮箱形态）
- 合法的结构化消费通道（不走 shell）：`.when(@proc.field OP v)`、契约 invariants、`write(content=@ref)` 落盘后 `cat`

> LLM 输出含单引号会炸 shell 引号结构——这是物理闸存在的根因，不是风格建议。

### 1.7 已退役语法（解析期警告 + 忽略，不入 AST）

`.cost(latency=…)` / `.check(result => …)` / `.ensure(result => …)`。
替代：cost → `.eval` 策略文件 + `--policy`（§3.4）；质量门槛 → 独立 judge proc + `.when` 路由。
**不要在新管线中使用。**

---

## 2. CLI 命令参考

前置：`ductile <command> …`。退出码 0/1；`--json` 用于机器输出（similar/nodes/fts/discover）。

### 2.1 管线生命周期

| 命令 | 动作 |
|---|---|
| `ductile check <file>` | 解析 + 类型检查（不执行） |
| `ductile parse <file>` | 仅解析，展示 AST 摘要 + 同构提示 |
| `ductile graph <file>` | E-graph 结构：节点/边/e-class/融合命中/CSE 别名/并行组/关键路径 |
| `ductile run <file> [topic] [-- k=v …]` | 解析 + 检查 + 执行；`--policy f.eval` 挂策略；`--restrict-shell` 禁 run/sh/spawn |
| `ductile import <dir\|file>` | 批量导入 .pipeline 到库（目录递归） |
| `ductile version save/log/diff` | 管线版本快照（`~/.local/share/ductile/versions/<name>/`） |

### 2.2 检索与复用

| 命令 | 动作 |
|---|---| 
| `ductile search "q"` | proc 库 LIKE 检索（tags+name+desc） |
| `ductile fts "q" [--json]` | BM25 全文检索（相关度排序） |
| `ductile discover [--json]` | 跨管线同构 proc 组（tag 集相等） |
| `ductile compose <name> <desc> <#tags> …` | 按 tag 链组装管线 |
| `ductile learn [dir]` | 静态 tag 序列模式学习（频率 ≥2） |
| `ductile similar [--json] [dirs]` | 结构键对齐检索 |
| `ductile build` | 从材料构建 |
| `ductile cost` | cost 相关操作 |

### 2.3 超网络（.hyper）

```bash
ductile hyper parse <file.hyper>
ductile hyper build <file.hyper> [-o out.pipeline]
ductile hyper check <file.hyper> <file.pipeline>
ductile hyper similar <file> [--json] [dirs...]
ductile hyper nodes <file>[:node]|--role X [--op Y] [--json] [dirs]
```

```
HyperGraph("name")
  .goal("…")
  .require(judge=true, min_impls=1)
  .vertex("a", role=source)
  .vertex("j", role=judge)
  .hedge("flow", kind=chain, a, b, c)
  .hedge("q", kind=gate, judge=j, producers=b, consumers=c)
  .deliver(c)
```

| kind | 含义 |
|---|---|
| `chain` | 有序数据路径 |
| `gate` | judge=/producers=/consumers=（consumer 用 `.when(@judge.…)`） |
| `bundle` | 共现 |
| `xor` | 互斥方案（slot + 多 impl） |

`hyper check` 语义：stage 名**精确等于** proc 名；chain 边要求下游 body 含 `@ref`；gate 要求 `.when(@judge.…)`.
写图前 `hyper similar` 查重，写节点前 `hyper nodes`。

### 2.4 探索环（explore）

```bash
ductile explore <file.pipeline> <topic> [--drs]
ductile explore --report <id>        # 只读检索冻结报告
```

出题（curriculum LLM）→ 沙箱真跑（`DUCTILE_DATA` 隔离；`cmd:` = shell 探针，`*.pipeline` = 管线探针）→ 确定性裁判（`exit 0` / `exit N` / `含 X` / `不含 X`，分号连接）→ Fail 固化 incidents（action/condition/consequence 三元组，err_code=explore_finding）→ 报告冻结 `$DUCTILE_DATA/explore/`。预算 8 波/12 深步；`--drs` 只深探。前置：`[agents.curriculum]`。

### 2.5 认知层命令

| 命令 | 动作 |
|---|---|
| `ductile canary list/add/rm/pass` | 已知好输入库（归因判别面） |
| `ductile incident list/close/triage` | 事故单生命周期（open→close） |
| `ductile l4 status/list/review/label/calibrate` | 端到端复核（log-only→enforcing） |
| `ductile shelve list/resolve` | 判别实验模糊搁置队列 |
| `ductile degraded [clear <name>]` | degraded 标志管理 |
| `ductile patch <pl> <proc> <impl> <field> <val>` / `list` / `clear` / `revert` | 运行时覆盖（不改源文件）；origin 出处标注 |

### 2.6 脚本契约与维护

| 命令 | 动作 |
|---|---|
| `ductile script attach/list/show/call/detach` | 脚本契约注册与调用（§7） |
| `ductile script doctor` | 逐条 stat 契约路径，DEAD → exit 1（只读） |
| `ductile archive` | wal_checkpoint(TRUNCATE) + 整库拷贝 `$DUCTILE_DATA/archive/`，保留 10 份 |
| `ductile db-stats` | 库统计 |
| `ductile wrap/harvest/promote/grow/scaffold` | 命令收割/构式生长线（数据源为终端历史） |

### 2.7 工程周期（本仓库）

`./devcycle.pipeline "start: …"` → 改代码 → `./devcycle.pipeline "feat: …"`；提交走 `./ship.pipeline "msg"`（test 门禁→add→commit→push→verify）；自测走 `./selftest.pipeline`。

---

## 3. 执行引擎行为

### 3.1 执行流水

```
apply_patches(pl) → build_egraph(pl) → parallel_groups(eg) → 逐层执行（层内串行）
```

### 3.2 路径选择（exec_proc 内）

```
eligible = plan.filter(when 通过)
ranked   = eligible.sort_by(score)
score    = base_cost × (1 + penalty) / pref
```

- `base_cost = 0.001×latency + 10×risk + 0.0001×tokens + 1×money`（静态声明通道已退役，全 0）
- 同 cost 平手由 `(proc, impl)` 字典序 tiebreak 决胜；**真正起作用的排序通道是历史惩罚/ImplPrefs 学习偏好**（`.pick(by=history)` 默认）
- `pref` 乘性学习权重：成功 ×1.1 / 失败 ÷1.5，clamp [0.05, 20]，落 impl_prefs 表

### 3.3 滑动窗口惩罚（窗口 20 次）

| 条件 | penalty |
|---|---|
| 无记录 / 窗口 <3 / 失败率 ≤10% | 0.0 |
| 失败率 >10% | e^(7×rate) − 1 |
| 连续失败 ≥3 | ∞（BLOCKED） |

### 3.4 评价策略（.eval，裁判分离）

```
ductile run x.pipeline "topic" --policy strict.eval
```

`.eval` 行式格式（`#`/`//` 注释）：`weights latency=0.001 risk=10.0 …`、`fail_closed = true`、`render.sdxl cost latency=measure("bench.sh {topic}")`。cost 两形态：直接数值 / `measure("cmd")` 实测（stdout 首个浮点数，latency 单位毫秒，cost_cache 表 TTL 24h）。未挂载 = 引擎默认权重 + `.plan()` 声明序。

### 3.5 运行测量（自动落库）

`rate_tokens` = 输出字符数/4；`latency_ms` = Instant 实测；`est_loss` 字段覆盖度函数（供 judge proc）。

### 3.6 e-graph 执行模式（默认关闭）

启用：任一 proc `.pick(egraph)` 或 env `DUCTILE_EGRAPH=1`。每 proc 一 e-class，equality saturation（union-only，上限 64 轮）：R1 同构合并 / R2 merge 扁平化 / R3 write-read 对消；**when-载体守卫**（挂 `.when` 的 class 不熔合——熔合抹裁判依赖序）；提取器 class 级 Kahn 拓扑 + CSE（同 class 只执行代表）。**FOPT 派生刀**：脚本 open incident → mcsm 派生 (2) → 禁熔合/禁 CSE（单向棘轮，incident 关闭回稳态）。

两层分工：e-graph 决定"谁跑"；exec_proc 决定"怎么跑"。熔合只影响静态提取计划，run 执行计数不变。

### 3.7 错误流（errflow，全自动零 DSL 面）

**错误分类 15 类**（判定链先具体后兜底）：

```
cancelled → contract → ratelimit → auth → timeout → memory → dependency
→ permission → network → resource → truncation → format → schema → data → crash
```

| 类 | 锚点 | strategy | respond |
|---|---|---|---|
| cancelled | SIGINT/KeyboardInterrupt | Escalate（禁自动重试） | Exit |
| contract | script not attached / param not in contract | Escalate | Exit |
| ratelimit | 429 / quota | Retry{2,linear} | Wait |
| auth | 401/403/invalid key | Escalate | Switch（换供应商） |
| timeout | run timed out | Retry{2,linear} | Wait |
| memory | CUDA OOM / exit 137 | Retry{1,linear} | Wait |
| dependency | ModuleNotFound / command not found | Escalate | Ignore |
| permission | EACCES/EPERM | Escalate | Ignore |
| network | conn refused / DNS / 5xx | Retry{2,linear} | Wait |
| resource | file not found / ENOSPC | Retry{1,linear} | Wait |
| truncation | finish_reason=length | Retry（重采样） | Wait |
| format | JSONDecodeError | Reroute | Switch |
| schema | KeyError/TypeError | Reroute | Switch |
| data | ValueError/IndexError | Reroute | Switch |
| crash | 兜底 | Escalate | Ignore |

**失败是值**：proc 失败不中止管线，`results[p] = §§FIELDS§§err=1§§err_code=…§§`（Left）；下游死源引用且无可切换 → 传播 Left（err_code 继承）；任何 Left → run exit 1。

**关键性判定（critical_set）**：`.deliver(@x)` 引用闭包 BFS = 主产出链 = 关键节点（Retry 预算 ×2，Left 致命 `critical proc 'x' failed`）；旁路失败容忍（Success + `bypass failures tolerated`）。无 deliver = 全关键。

### 3.8 认知上下文合成（auto-prompt）

`llm(agent)` 不写 `prompt=` 时，引擎按节点图位置自动合成八段 prompt：身份 → 主题 → 上游输入（§§FIELDS§§ 预览，截 200/2000 字符双窗——约束敏感下游宽窗）→ 继承约束 → 下游消费者 → guide（`[agents.x]` 开放动作）→ 错误记忆（open incidents 指针卡 ≤3）→ 输出契约。历史统计（近 20 次）≥3 条注入。fail-closed：无 agent 或身份信息全空 → 硬错误。

**上下文协商（negotiate，declare-then-run）**：`[agents.x] negotiate=true` 或 env NEGOTIATE=1（默认关）。模型输出 `{"enough":false,"missing":[{"ref":"@x.y","why":…}]}` → 引擎按解析表补料追加式重跑（预算 3 轮 fail-closed）。协商日志落 runs.negotiation；协商中间轮禁播 canary。

---

## 4. 配置（config.toml）

查找顺序：`$DUCTILE_CONFIG` → `./config.toml` → `./ductile.toml` → `~/.config/ductile/config.toml` → `~/.local/share/ductile/config.toml`。

```toml
[llm]                    # 连接层（base_url/api_key 永远在此，不属于 agent 语义）
base_url = "http://localhost:11434/v1"
api_key  = "ollama"
model    = "qwen3.8:27b"

[models.light]           # 命名档位（model 必填；base_url/api_key/timeout 回落 [llm]）
model = "qwen3.8:9b"
max_tokens = 4000        # 思考型模型预算，覆盖 OPENAI_MAX_TOKENS

[agents.analyst]         # 语义角色（system/schema/guide/timeout_secs/tiers/negotiate）
system  = "…（\\n 展开为真换行）"
schema  = "sid int, pass bool"
tiers   = "light,medium,high"   # 升序阶梯
guide   = "检索方式/命令/示例"
negotiate = false

[agents.curriculum]      # explore 出题角色（必需，硬文法）
```

**档位选择三信号**：`tier=` 实参（最高）> 复杂度打分（schema 字段数 ≥3/≥6、prompt >1200/>4000、system >400）> 单档钉死。**失败升级**：档 i 失败自动升 i+1 到顶（有界）；`model=` 实参完全旁路。阶梯引未定义档 / tier 不在阶梯 → 硬错误。结果附 meta_tier/meta_model_id。

**llm 合并序**：显式实参 > agent 配置 > `[llm]` > 内置默认。

---

## 5. LLM 集成

- 桥：仓库内 `bridge/llm_bridge.py`（OpenAI 兼容 API）
- **OPENAI_MAX_TOKENS 必设**：bridge 不传 max_tokens 时本地服务默认 ~200 token 截断——长 JSON 尾部字段静默丢失且管线全绿。长任务前 `export OPENAI_MAX_TOKENS=4000`
- 环境变量 `OPENAI_BASE_URL` / `OPENAI_API_KEY` / `OPENAI_MODEL` > config.toml `[llm]` > 内置默认
- **影子桥劫持**：repo 外运行 llm() 会落到旧遗留桥（位置参数式）。修法：`cp bridge/llm_bridge.py ~/.local/share/ductile/bridge/` 保持同步
- 裁判独立性：producer 与 judge 同模型会橡皮图章；judge 的 schema 数值字段排前、评语排后（截断保数值）

## 6. DSL_RESULT 协议（结构化数据流）

```
##DSL_RESULT
key1=value1
##DSL_END
```

任何 run/sh/llm(schema) 的 stdout 末尾带此块 → 自动解析为结构化字段，下游 `@proc.field` 引用。内部编码 `§§FIELDS§§k=v§§RAW§§原始stdout`。无块时整体 stdout 为结果（>5000 字符截断）。

## 7. 脚本契约（脚本即 API）

### 7.1 契约头（脚本头部注释行）

```
# ductile: v1
# name: word_stats
# desc: 一句话
# lang: python              ← bash | python | powershell
# params: text(str, required), n(int, default=10)
# output: words(int), lines(int)
# pure: true                # 副作用标注（cse_safe 判定用）
# idempotent: true
# concurrency: safe         # safe | exclusive
# effects: none             # none | fs | net | system
# timeout: 10               # 秒；# retries: N 可选
# mcsm: F(2)-O(1)-P(3)-T(2)          # 可选 FOPT 坐标
# mcsm_note_f: 本机文件系统+PATH coreutils   # mcsm 声明时四维注解必填（裸通用名拒收）
```

`pure && idempotent && concurrency=safe` → `cse_safe=true`（可 CSE/并行）；副作用脚本不可熔合不可并行。

### 7.2 DSL 调用与执行语义

```
.proc("analyze")
  .plan(py -> script(word_stats, text="{topic}"))
```

- 未注册脚本名硬错（列已注册清单）；必填参数缺失/空硬错；契约未声明的幻觉参数硬错
- 传参：环境变量 `DUCTILE_ARG_<NAME>` + `DUCTILE_TOPIC`（**不是 argv**）
- 值支持 `{topic}`、`@proc.field`、契约 default；env 注入前引擎剥一层对称包围引号
- 输出复用 ##DSL_RESULT；timeout/retries 走契约头

### 7.3 脚本写作四律

1. bash 必须 `set -euo pipefail`（否则中间失败被吞、末尾 echo 返回 0 → 静默半成功）
2. 机器可读走 ##DSL_RESULT，人类可读走 stderr
3. pure/concurrency 如实标注（撒谎会让编排优化破坏正确性）
4. 幂等脚本注明 idempotent: true

## 8. 认知层（契约卡 / canary / incident / L4 / shelve）

### 8.1 节点契约卡

```
.proc("judge")
  .plan(j -> run("judge.sh {topic}"))
  .contract(outputs="score, note", invariants="@self.score >= 80")
```

执行后确定性校验（零 LLM）：outputs 字段存在性（L1）+ invariants 谓词（L2）。违例 → Exit 不重试 → 自动落 incident。

### 8.2 五分类归因闸

| class | 归因 | 判定 |
|---|---|---|
| 1 | 环境问题（桥/网络/凭证） | 确定性终态，零 LLM |
| 2 | 上游投毒 | canary 绿（canary 过 + 真实输入挂） |
| 3 | 描述欠约束 | canary 红 + 判别实验：desc 修订后质变 |
| 4 | 模型能力不足 | canary 红 + 判别实验无改善 |
| 5 | 世界真变了 | 外部变化证据，准入最严 |

硬门禁：**无 canary 通过记录禁止本地 patch**。

### 8.3 分层信号（L0-L4）

L0 基础设施（errflow 分类）/ L0.5 截断 / L1 字段缺失（契约 outputs）/ L2 谓词违例（invariants）/ L3 统计漂移（历史分布）/ L4 端到端意图（独立复核，唯一合法 LLM 检测层）。确定性证据短路 LLM。

### 8.4 canary 矛盾态禁播（X3）

proc 有 open incident 期间禁止归档 canary——矛盾期一次侥幸成功会被播成"已知好输入"洗白坏节点。自动播种静默跳过；`canary add` 显式报错指引 `ductile incident close <id>`。关闭自动恢复。

### 8.5 L4 冷启动与升格

```
log_only ──(≥8 标签 且 一致率 ≥70%)──▶ enforcing
```

`DUCTILE_L4=1` 开启管线收尾自动复核（Success→pass，Failed→fail）。

## 9. 上下文协商（negotiate）

`[agents.x] negotiate=true` 或 NEGOTIATE=1（默认关）。模型输出 `{"enough":false,"missing":[{"ref":"@x.y","why":…}]}` → 引擎补料重跑（追加式，预算 3 轮 fail-closed）。resolve 支持 @proc.field / @proc 全文 / topic / script:契约卡。日志落 runs.negotiation；中间轮禁播 canary。

## 10. SQLite schema（18 表）

库：`$DUCTILE_DATA/ductile.db`（未设 → `~/.local/share/ductile/ductile.db`）。`DUCTILE_DATA` 同时是隔离探针开关。核心表：

| 表 | 用途 |
|---|---|
| pipelines / procs / runs / compositions | 管线注册与执行记录（runs 含 latency_ms/rate_tokens/est_loss/negotiation） |
| patches | 热补丁（origin 出处：human / llm:\<model\> / machine） |
| impl_prefs | 乘性学习权重 |
| cost_cache | measure 实测缓存（TTL 24h） |
| scripts | 脚本契约卡（含 mcsm/mcsm_note） |
| canaries / incidents / l4_reviews / shelved | 认知层四件 |
| wrapped_cmds / promotions / scaffolds | 命令收割/构式生长 |

新增表必须进 SCHEMA_DDL 单一事实源（懒建表旁路是历史坑）。完整 DDL 见 `src/L0_physical/db.rs` 的 SCHEMA_DDL。

## 11. 模块结构（七层认知栈）

```
src/
├── interface/        # cli（命令分发）、api（pyo3 32 pub fn）
├── core/             # ast、dslresult、script_card（不依赖任何层）
├── L4_structure/     # hyper/learn/grow/promote/harvest
├── L3_dsl/           # parser/typecheck/when/config/version
├── L2_orchestration/ # executor/steps/egraph/eval/script/registry/textargs/ranking
├── L1_feedback/      # errflow/canary/incident/l4/shelve
└── L0_physical/      # db + 三张 schema.sql
```

依赖铁律：只许向下（interface > L4 > L3 > L2 > L1 > L0；core 任意）。`scripts/layers_probe.py` 探针守护（selftest 门禁），违规即红。

**测试**：`cargo test --lib`（476 项）。db 层双形态注入（`*_conn` 内核 + 全局薄壳）。

## 12. API 层（core + 薄壳双形态）

- 所有 API 是 `*_core`（纯 Rust）+ pyo3 `#[pyfunction]` 薄壳；**bin/test 必须调 `*_core`**（pyo3 符号拉进 bin 链接图会炸 rust-lld）
- 语义：创作错误 → Err/异常；执行失败 → `Ok({"ok":false,"error":…})`（失败是数据）
- LangChain：`ductile.langchain_tools()`（python/ 混合包）——6 内省/执行工具 + 每脚本契约一个类型化工具
- PyPI 发布：`maturin build --release -o dist` + twine（token 见 `~/.config/ductile/pypi-token.sh`）

## 13. LLM 管线配方（实战沉淀）

### 13.1 五段生成链（基座）

```
req(需求分析) → arch(架构) → brk(任务拆解) → audit(审计门)
```

每段一个 `[agents.x]`（schema 强制结构化），`.when(@上游.字段)` 串联，audit 挂 `.needs(@req)`。

### 13.2 问询链（防臆造——最重要）

req 与 resolve 两节点，中间"下级提问上级裁决"：

```
.proc("req", llm(req_analyst))                     // schema 含 open_questions
.proc("resolve", llm(resolver, prompt="原始描述：{topic} ||| 问题：{@req.open_questions}"))
  .when(@req.must_have)
  .constraint(resolved_constraints)                // 干净约束全链继承
.proc("arch", llm(architect)).when(@resolve.resolved_constraints)
.proc("brk", llm(breaker)).when(@arch.modules)
.proc("audit", llm(auditor)).when(@brk.tickets).needs(@req)
```

resolver 的 system 写死两类推断纪律：限制性推断（须原话明确禁止才成立）/ 可执行化推断（应收录）。实证：27B 问询链 82.7 vs 裸 67.7（+15）。

### 13.3 owner 槽位

任务拆解类 schema 的每张 ticket 内嵌 `owner` 字段（不放顶层），guide 给白名单。实证弧线：-16.3 →（.constraint）-9.7 →（+owner）**+14.7 反超**。

### 13.4 提示词五律（9B 档实测）

① 数量锚定有效 ② 字段清单要全 ③ desc 负重要轻（30-60 字+示例是甜剂量）④ 算术指令反噬（对账交引擎）⑤ 结构修正边际递减。**guide 详尽度与模型能力反相关**：加压 guide 对 9B +21.3，对 27B -4.0——27B 档位正确姿势：约束递到手、留好 owner 槽位、然后闭嘴。

### 13.5 模型档位（单 seed 量级参考）

| 档位 | s2 盲评 | 定位 |
|---|---|---|
| 裸 27B | 66.7-79.3（波动大） | 对照臂 |
| 27B 问询链 | 82.7 | 主力 |
| 9B 加压 guide | 61-64（触顶） | 提问臂+粗拆解 |
| 8B 问询链 | 32-36 | 不可用于深推理 |

> 证据等级：单 seed 历史实测（harness 已失传，复跑需重建 3-seed 起）。引用视作量级参考非精确对比。

## 14. AI 调用者速查（写管线前必读的坑）

1. **抄范本，禁止凭记忆**——`examples/` 与 `pipelines/` 是正典
2. 多行 @ref 必须引号包裹：`echo "@scan" | grep x`（裸替换管道符掉行首 → bash 语法错）
3. write() 的 `\n` 是字面量两字符，不转义；要换行拆 impl 或 printf
4. 一个 proc 只暴露获胜 impl 的值；两个统计量拆两个 proc
5. 管道末位命令决定 exit code：计数用 `find … | wc -l`
6. 未声明 cwd 的管线 run() 必须绝对路径；bash 多行 for/if 需 `bash -c` 包装
7. 无 @ref 的 proc 是旁路会并行抢跑——顺序依赖用 `.when(@upstream.field OP v)`（引擎内求值），不要 `echo '@x' >/dev/null` 占位
8. 依赖门禁模式：`echo '@test' | grep -q '绿标' && git commit … || echo skipped`
9. LLM 输出一律不进 bash（§1.6 trust 闸 + 血泪规则）
10. `run()` 结果是 Text 无字段 → `.when(@x.ok)` fail-closed 判死——先让上游吐 ##DSL_RESULT

**错误排查表**：

| 现象 | 排查 |
|---|---|
| Parse error | 括号/逗号/`->`；字符串字面量内的括号已跳过 |
| `bad .when(@x) … fail-closed` | 裸 @proc 缺 `.field`，check 期拦截 |
| `shell injection of @x requires explicit trust` | 补 `.trust(@x)` |
| All paths failed | 检查 bridge 路径 / 桥劫持（§5） |
| 路径总不被选中 | penalty 惩罚，`ductile patch list` |
| 结构化字段为空 | 脚本没输出 ##DSL_RESULT 块 |
| `script 'X' not attached` | 先 `ductile script attach` |
| `param 'k' not in contract` | 幻觉参数，`script show X` 对契约 |
| agent ladder references undefined tier | config 缺 `[models.<tier>]` |
| schema requested but no JSON | 小模型被重 guide 压垮——降 guide 或加兜底档 |

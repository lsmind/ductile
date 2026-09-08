# Ductile（铸渠）总体设计文档

> 版本 **v0.13.1** · 2026-09-07 整理 · ~13.4k LOC / 325+ Rust 测试 · 含 v0.14 errflow
> 配套文档：README.md（产品视角）· SPEC.md（API 规格，供 AI 调用者）· docs/v0.11_decouple_spec.md · docs/v0.14_error_flow_spec.md

---

## 1. 项目定位

**一句话**：声明式 Agent 流水线引擎——用户只声明"做什么"和"有哪些备选路径"，引擎自动完成路径选择、失败降级、质量门槛、重复消除与经验学习。

**要解决的问题**：Agent 编排充满不确定性（哪条路能跑通？失败了怎么办？哪条路更好？）。传统做法靠人工 if/else 与手调参数；Ductile 把不确定性管理变成引擎的自适应数值优化。

**与同类工具的分工**：

| 工具 | 解决的问题 | 缺什么 |
|------|-----------|--------|
| LangGraph / AutoGen | 怎么连起来（代码+图） | 多路径降级、自学习 |
| Airflow / Prefect | 任务调度（外部 DB） | 语义等价消解、零依赖 |
| 传统路由层 | 选哪个模型（静态规则） | 编排、历史学习 |
| **Ductile** | **选择+降级+自学习+热补丁+零改接入**，一个内嵌 SQLite 的单文件引擎 | — |

---

## 2. 设计哲学（不可妥协的原则）

1. **声明意图，不声明控制流**——不写 if/else/try/catch，声明路径与优先级，引擎负责排序、降级、短路。
2. **裁判分离（v0.11 定稿）**——"不能又当运动员又当裁判"。流程文件不自带判罚；评价与流程三处解耦：语法层（`.eval` 策略文件）、运行层（`--policy` 挂载）、数据层（measure 实测 + 24h 缓存）。
3. **失败是数据，不是异常**——执行失败返回 `{"ok":false,"error":...}`，agent 按 JSON 路由；创作错误（解析/类型检查）才抛异常。
4. **fail-closed 默认**——未知函数、坏 when 条件、裁判缺席、幻觉参数、未注册脚本，一律不放行。
5. **机制替代意志力**——失败率 >10% 指数惩罚、连续 3 次失败自动 BLOCKED，不靠人盯。
6. **引擎审计不信注释**——parsed-but-never-consumed 反模式反复出现；声称的特性必须 grep 调用点 + 真跑验证（历史教训：SPEC 曾写 `weights(rd=N)` 语法但从未实现，旧版还把它吃成幽灵 impl 假成功落库）。

---

## 3. 总体架构

```
┌────────────────────────────────────────────────────┐
│ CLI (cli.rs ~1080) —— check/run/parse/graph/stats  │
│   import/search/discover/learn/db-stats            │
│   patch/version/promote/grow/scaffold/harvest…     │
├────────────────────────────────────────────────────┤
│ 集成层  api.rs(~700) core+pyo3薄壳双形态            │
│         lib.rs → python/ductile（LangChain 工具）   │
├────────────────────────────────────────────────────┤
│ DSL 层  parser(~1400) / ast(~390) / typecheck      │
│         when(~390, Interpreter 条件路由)            │
├────────────────────────────────────────────────────┤
│ 编排层  executor(~870) / ranking / eval /          │
│         egraph(~1225, union-only 等价类+CSE) /     │
│         errflow(~1120, v0.14 错误分类与传播)        │
├────────────────────────────────────────────────────┤
│ 执行层  steps(~1200, 内置算子+进程表) /             │
│         textargs / dslresult（##DSL_RESULT）       │
├────────────────────────────────────────────────────┤
│ 语义层  script（v0.12 脚本契约：脚本即 API）         │
├────────────────────────────────────────────────────┤
│ 知识线  registry / learn / harvest / promote /     │
│         grow / version                             │
├────────────────────────────────────────────────────┤
│ 数据层  db(~1180, SQLite, *_conn 注入内核)         │
└────────────────────────────────────────────────────┘
```

约 23 模块、~13.4k 行 Rust。`parser`/`egraph`/`db`/`steps`/`errflow`/`cli` 为主要体量；executor 在 v0.12.1 从巨石拆到纯编排，v0.14 错误值化与默认路径/egraph 路径终局语义对齐（`fatal_left`）。

---

## 4. 核心机制设计

### 4.1 声明式 DSL（.pipeline）

纯文本，`//` 注释。一个 proc = 一个工序节点，`.plan()` 内多条 `name -> body` 路径互为备选：

```
Pipeline("research")
  .proc("search")
    .plan(
      web -> web_search(query="{topic}").tags(#search, #web).retry(n=3),
      mcp -> mcp_search(query="{topic}").tags(#search, #mcp)
    )
  .proc("gate")                          // 独立裁判
    .plan(g -> run("judge.sh {topic}").tags(#judge))
  .proc("write_report")
    .when(@gate.score < 80)              // 块级 when：废品不进门
    .plan(w -> write(to="~/out.md", content=@search))
  .proc("deliver")
    .deliver(@write_report)              // 终端标记，不执行
```

- 依赖自动推导：body 中 `@proc_name` 引用即声明依赖；`ductile graph` 可看拓扑/并行组/关键路径
- 变量替换：`{topic}`（执行前）、`{var}`（foreach 迭代）、`@proc`（上游全文）、`@proc.field`（结构化字段，依赖完成后）
- `.when()` 两种写法（v0.11.1）：内联（impl 级）与块级（proc 级，下推全部 impls，内联优先）；块级条件的 `@ref` 自动并入依赖图，保证裁判先执行；语法错误是解析期硬错误（不再静默丢弃）
- **v0.11 退役语法**：`.cost()/.check()/.ensure()` 仍可解析但警告+忽略（不入 AST），不要在新管线使用

### 4.2 路径选择与自学习（ranking.rs）

```
score(impl) = base_cost × (1 + penalty) / pref
base_cost   = 0.001×latency + 10×risk + 0.0001×tokens + 1×money
pref        = 乘性学习权重（成功 ×1.1 / 失败 ÷1.5，clamp [0.05, 20]）
```

滑动窗口惩罚（每 impl 保留最近 20 次）：失败率 ≤10% 无惩罚；>10% 触发 `e^(7×rate)−1` 指数惩罚；连续 3 次失败 → 永久 BLOCKED。窗口 <3 次为冷启动，按声明顺序探索。penalty 只管瞬时故障（可 retry 恢复的），偏好 pref 持久化 `impl_prefs` 表跨会话生效。

### 4.3 裁判分离（v0.11 — eval.rs）

`.pipeline` 只描述流程；权重与 cost 来源写在 `.eval` 策略文件：

```
weights latency=0.001 risk=10.0 tokens=0.0001 money=1.0
fail_closed = true
render.sdxl cost latency=measure("bench_sdxl.sh {topic}")   # 实测形态
render.flux  cost latency=5000.0                             # 直接数值
```

- `measure("命令")`：引擎执行脚本取 stdout 第一个浮点数（latency 单位毫秒），按 `(proc, impl, field)` 缓存 `cost_cache` 表，TTL 24h
- 质量门槛新形态：独立 judge proc 输出 `##DSL_RESULT score=N` + 下游 `.when(@judge.score < 80)` 路由——**产出者不自证清白**；`.when` 求值 fail-closed（坏条件/裁判缺席/字段非数值 = 不放行，deliver 无可用 impl → degraded + exit 1）
- 退役清单：`.cost/.check/.ensure`（警告+忽略）、`<noop>` 假成功（改硬错误）、`.plan()` 无名条目（改硬错误）、RD 附加费/est_loss 排序项（已删，`latency_ms` 改 Instant 实测落库）

### 4.4 e-graph 等价类（v0.10 — egraph.rs）

`.pick(egraph)` 或环境变量 `DUCTILE_EGRAPH=1` 开启。多路径选择的数学本体从"贪心排序"升级为"等价类提取"：

1. 每 proc 一个 e-class，impl 投影为 e-node `(op, children)`
2. **equality saturation**（union-only，canonical class 数单调下降必终止，上限 64 轮）：
   - R1 同构合并：两 class 的 canonical 节点集一致 → union
   - R2 merge 扁平化：涵盖交换律/嵌套折叠/退化
   - R3 write→read 对消：写后读融合为 pass-through
   - **when-载体守卫（v0.11.1）**：挂 `.when` 的 impl 不参与任何熔合——熔合会抹掉裁判依赖序（实测坑：echo 形态的 gen/deliver 同构熔合导致 deliver 抢跑、判决落空）
3. 提取：class 级 Kahn 拓扑 → 逐 class 选最小静态 cost 的 enabled impl
4. **CSE**：同 class 只跑代表，其余共享结果（5 procs 实测压成 3 classes）

两层分工：e-graph 决定"谁跑"（代表/序/别名），exec_proc 内部按历史惩罚决定"怎么跑"（impl 序 + retry）。默认关闭，不声明时行为与 v0.9 完全一致。

### 4.5 ##DSL_RESULT 协议（dslresult.rs）

零改接入的根基：任何脚本 stdout 末尾打块即可返回结构化数据：

```
##DSL_RESULT
path=/tmp/out.mp4
score=85
##DSL_END
```

内部编码 `§§FIELDS§§k=v§§RAW§§原始stdout`，下游 `@proc.field` 直接引用；字段提取有 RAW 边界（不越界进原始 stdout）。api 层对外输出时解码为干净 `fields` 对象。

### 4.6 脚本契约（v0.12 — script.rs，"脚本即 API"）

脚本本体不写入 DSL，DSL 只留 `script(name, k=v...)` 链接；元数据经脚本头部 `# ductile:` 契约头自描述：

```
# ductile: v1
# name: word_stats
# lang: python
# params: text(str, required), n(int, default=10)
# output: words(int), lines(int)
# pure: true  /  idempotent: true  /  concurrency: safe  /  effects: none
```

- **语义标注喂编排**：`pure && idempotent && concurrency=safe → cse_safe=true`（可 CSE/并行）；副作用脚本不可熔合——防同构副作用节点被 CSE 熔成一个、双执行变单执行
- CLI：`script attach/list/show/call/detach`；LLM 调用只读契约卡（`script show`），不读脚本体
- 执行语义 fail-closed：未注册脚本硬错（列出已注册）、必填参数缺失/空硬错、**契约未声明的幻觉参数硬错**；传参走 `DUCTILE_ARG_<NAME>` / `DUCTILE_TOPIC` 环境变量；timeout/retries 走契约
- 脚本写作规范第 1 条（实测坑）：bash 必须 `set -euo pipefail`，否则中间失败被吞、引擎收到 exit 0 → 静默半成功

### 4.7 API 层与 LangChain（v0.13 — api.rs + python/ductile）

- **core+薄壳双形态**：全部 API 是 `*_core` 纯 Rust（String/Result），pyo3 `#[pyfunction]` 薄壳只做包装。bin/test 内部必须调 `*_core`——直调 pyfunction 会把 pyo3 运行时拉进链接图，而 `extension-module` 不链 libpython（实测 rust-lld `undefined symbol: _Py_Dealloc`）。core 路径下 pyo3 代码被 `--gc-sections` 丢弃，bin 与 wheel 各自干净
- **失败是数据**：创作错误 → Err/异常；执行失败 → `Ok({"ok":false,"error":...})`
- **LangChain**：`ductile.langchain_tools()` 一行接入——6 个内省/执行工具 + 每个已注册脚本契约一个类型化工具（docstring/参数 schema 从契约卡自动生成，LLM 不读脚本体）。实测踩坑：Pydantic v2 保留名 `args` 被别名化（改用 `kv`）；`@tool` 装饰前须有 docstring；`**kwargs` 无 schema（`inspect.Signature` 注入）
- Web 前端已删（v0.13.1 用户裁定"不成熟"）：serve.rs + web/ 移除，git 历史 `6d3c00e` 可找回；HTTP API 面已由 LangChain + pyo3 覆盖

### 4.8 知识库学习线（自组织）

| 机制 | 命令 | 原理 |
|------|------|------|
| 同构发现 | `discover` | 两 proc 的 tag 集合完全相等 → 同构；跨 pipeline 提示 |
| 静态学习 | `learn` | 扫描 tag 序列，发现频率 ≥2 的重复模式（不依赖执行） |
| 命令收割 | `harvest` | wake-phase 传感器：记录真实执行命令进 wrapped_cmds |
| MDL 晋升门 | `promote` | 两部码检验（晋升成本 vs 次数×节省）→ 自动铸成 proc；**计数单位是跨会话数**（≥2 才是复用证据，会话内重复只是调试迭代）；账本表可审计可回滚 |
| 构式生长 | `grow` | 60d 命令全文语料上贪心 pair-merge，多行构式入 scaffolds |
| 组合 | `compose` | 按 tag 链从零件库组装新 pipeline |
| 版本快照 | `version save/log/diff` | `~/.local/share/ductile/versions/` |
| 热补丁 | `patch` | 不改源文件覆盖 enabled/cost/retry/stub，UPSERT patches 表，run 时 apply |

---

## 5. 数据层（db.rs）

单文件 `~/.local/share/ductile/ductile.db`，零外部服务。9 张表：

`pipelines` / `procs` / `runs`（含实测 latency_ms、rate_tokens、est_loss）/ `compositions` / `patches` / `impl_prefs`（偏好学习）/ `cost_cache`（measure 缓存 TTL 24h）/ `scripts`（v0.12 契约卡）/ `wrapped_cmds` + `promotions` + `scaffolds`（自组织线）。

工程模式：数据函数双形态——`*_conn(conn)` 内核（测试注入内存连接）+ 全局薄壳（生产路径）；表结构改动只改 `SCHEMA_DDL` 常量（init_db 与测试共用单一事实源）。

---

## 6. 工程质量

### 6.1 测试架构（325+ Rust + 11 pytest，三层）

| 层 | 对象 | 手法 |
|----|------|------|
| 纯函数直测 | 解析原语/协议编解码/参数解析 | 无 I/O，断言输入输出 |
| 内存库回路 | db 层 22 函数 | `Connection::open_in_memory` + SCHEMA_DDL |
| 真实子进程集成 | run/spawn/kill/wait/fs 算子 | 临时目录 + setsid 进程组隔离 |

一条命令全跑：`cargo test --lib`。覆盖率工具（tarpaulin）与 pyo3 debug 链接不兼容，以测试面清单为准。

### 6.2 模块化解耦（v0.12.1，测试 162 → 262）

executor 巨石拆四模块（textargs/dslresult/steps/ranking，re-export 兼容旧路径）、db 层 22 函数 `*_conn` 注入、cli 抽纯参数解析、harvest/parser/egraph 原语直测。**解耦过程暴露并修复 6 个潜伏真 bug**：

1. `exec_spawn` 未自立进程组 → 组杀静默无效
2. `exec_wait`/`exec_procs` 以 `/proc` 存在性判活 → 僵尸进程死等超时
3. `extract_field` RAW 终止符死分支 → 字段提取越界进原始 stdout
4. executor 残留 v0.11 迁移死代码岛 ~135 行
5. `parse_promote_args` 类型分支派发 → 第二位置参数从未生效
6. `jparse_str` UTF-16 代理对 off-by-one → 串尾代理对返回 None、后随字符被吞

### 6.3 已知坑速查

- `run()` 不继承 pipeline 文件 CWD，路径一律绝对路径；bash 多行需 `bash -c` 包装
- spawn 自立进程组 + `/proc/pid/stat` 僵尸感知（v0.12.1 修复后语义）
- e-graph 熔合会抹裁判依赖序 → when-载体守卫（勿关）
- tarpaulin × pyo3 不兼容；rust-lld × pyfunction 直调 = undefined symbol
- Pydantic v2 参数名 `args` 被别名化；`@tool` 装饰前须有 docstring

---

## 7. 版本演进

| 版本 | 主题 | 关键交付 |
|------|------|---------|
| v0.7 | 资源管理算子 | spawn/procs/kill/wait + fs 算子 + 安全边界 |
| v0.8.1 | harvest | 命令收割传感器 |
| v0.9.x | 自组织 | promote（MDL 晋升门）/ grow / scaffold |
| v0.10 | e-graph | 等价类 + equality saturation + CSE |
| v0.11 | **裁判分离** | .eval + --policy；退役 .cost/.check/.ensure；measure + cost_cache；fail-closed |
| v0.11.1 | when 完善 | 块级 when + 硬错误 + e-graph when-载体守卫 |
| v0.12 | 脚本契约 | 脚本即 API；契约头/契约卡/cse_safe；script CLI 族 |
| v0.12.1 | 模块化 | executor 拆分 + db 注入 + 测试 162→262，修 6 潜伏 bug |
| v0.13 | API/集成 | api.rs core+薄壳；LangChain 插件；实测端到端 |
| v0.13.1 | 收敛 | 删 Web 前端与 serve（用户裁定）；测试 325+；文档全对齐 |
| v0.13.1+ | 硬化 | Windows 可编译；egraph 失败语义对齐 fatal_left；typecheck 未知函数；CI |

## 8. 当前状态与边界

- **代码**：~23 模块 ~13.4k 行；325+ Rust 测试 + 11 pytest；Linux CI
- **制品**：二进制 `ductile`；wheel 0.13.1（PyPI badge 已对齐；远端发布视 token）
- **明确不做**：Web 控制台（不成熟，已删可找回）；嵌入式脚本（DSL 只链接不嵌脚本，v0.12 裁定）
- **平台**：Linux 一等公民（bash/`process_group`）；Windows 可编译，shell 算子需 PATH 上有 bash
- **设计取舍备忘**：评价与流程分离是宪法级原则——任何把 cost/门槛写回 `.pipeline` 的提案都是倒退
- **安全**：默认允许 `run`/`sh`/`spawn`（本地可信）；`DUCTILE_RESTRICT_SHELL=1` 或 `--restrict-shell` 封锁 shell；`DUCTILE_UNSAFE_SHELL=1` 可覆盖。非多租户沙箱。
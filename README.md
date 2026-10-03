# Ductile

> 把一件需要好几步才能完成的工作，写成一个文件、一条命令跑完。
> 中间某步失败了？Ductile 会按声明的备选路径继续尝试。
> 管线能跑完，也能解释、审计和复用；你不必把可靠性散落在每个脚本里。

**版本**：本文对应 **0.23.0**（Cargo / pyproject / PyPI 三源同版）。`ledger`、`attractor`、`toon` 命令组均已随 0.23.0 发布。

第一次阅读只需掌握安装、一个 `.pipeline` 文件和 `.plan()`；账本与决策记忆放在后面的高级章节。

## 这是什么

Ductile 是一个用 Rust 编写的声明式管线 DSL。你描述“要做什么”，引擎负责检查、调度、执行、换路和留痕。

- **一个文件描述整件工作**：步骤、依赖、门禁、交付物都写进 `.pipeline`。
- **失败自动换路**：一个步骤可以声明多个实现，失败后自动尝试下一条路径。
- **结构化协作**：AI、命令和脚本产出结构化字段，下游按字段引用。
- **执行即留痕**：历史保存在本地 SQLite 单库中，可用于选路、排障和 TUI 查看。

`.pipeline` 既是配置，也是可读、可版本管理的流程文档。DSL 提供 **21 个内置动词**，包括 `run`、`write`、`llm`、`script`、`mcp` 等。

## 首次上手（约 5 分钟，不含构建）

### 1. 安装与 PATH

前置：Linux/macOS + Rust 工具链（首次 release 构建约数分钟）。

```bash
git clone https://github.com/lsmind/ductile.git
cd ductile
cargo build --release
```

构建产物位于 `target/release/ductile`。建用户级软链让任意目录都能调用（`$HOME/.local/bin` 通常已在默认 PATH；若不在，把 export 行写进你的 shell 配置——bash 用 `~/.bashrc`，zsh 用 `~/.zshrc`）：

```bash
mkdir -p "$HOME/.local/bin"
ln -sfn "$PWD/target/release/ductile" "$HOME/.local/bin/ductile"
export PATH="$HOME/.local/bin:$PATH"
command -v ductile
```

**版本核验**：跑 `ductile --version` 或 `ductile --help` 看命令清单——0.23.0 应含 `ledger` / `attractor` / `toon` 命令组。

### 2. 写第一条管线

新建 `hello.pipeline`：

```text
Pipeline("hello", "我的第一条管线")

  // 执行一条命令
  .proc("greet", run("echo hello ductile"))

  // 把上一步输出写入文件
  .proc("save", write(to="/tmp/hello.txt", content=@greet))

  // 声明最终交付物
  .proc("deliver")
    .deliver(@save)
```

`Pipeline(...)` 声明管线，`.proc(...)` 声明步骤，`@greet` 引用上一步输出，`.deliver(...)` 声明最终交付物。

```bash
ductile check hello.pipeline
ductile run hello.pipeline
cat /tmp/hello.txt
```

最后一条命令会输出 `hello ductile`；成功后也可以清理临时文件：`rm /tmp/hello.txt`。

### 3. 加上失败换路

`plan` 可以同时声明多个实现，并优先尝试第一个，失败后再尝试下一个。`plan` 不需要写条件判断；直接写 `plan(a, b, c)` 即可。

把管线改为 `fallback.pipeline`：

```text
Pipeline("fallback", "有备选路径的管线")

  .proc("prepare", write(to="/tmp/fallback-data.txt", content="data"))
  .plan(fallback,

    a,
    b,

    run("exit 1"),
    write(to="/tmp/fallback-data.txt", content="data")
  )
  .needs(@prepare)

  .proc("deliver")
    .deliver(@fallback)
```

检查并执行：

```bash
ductile check fallback.pipeline
ductile run fallback.pipeline
```

两个实现任一成功即可；`ductile check` 静态检查管线错误，`ductile run` 实际执行并应用 fallback。

## 常用命令

| 命令 | 用途 |
| --- | --- |
| `ductile check <文件>.pipeline` | 静态检查管线错误，不执行步骤 |
| `ductile run <文件>.pipeline` | 正式执行一条管线 |
| `ductile parse <文件>.pipeline` | 解析并输出结构信息 |
| `ductile graph <文件>.pipeline` | 导出依赖与拓扑视图 |
| `ductile db-stats` | 快速查看内核 DB 的运行与记忆统计 |

把 `ductile run <文件>.pipeline` 当作日常执行入口，把 `ductile check` 放进提交前或 CI 流程。

## 让 AI 参与

### 1. 声明角色与结构化输出（llm + schema）

AI 是管线里的一种"步骤"，和普通命令平起平坐。`llm(agent)` 的角色、提示词、输出字段都定义在 `config.toml` 的 `[agents.x]`；管线里强制结构化输出用 `schema`，下游用 `@步骤.字段` 引用：

```
  .proc("summarize", llm(analyst, prompt="把 {topic} 摘要", schema="title str, score int"))
```

`schema` 声明的字段格式崩了不放行、不污染下游——这是管线里 AI 与脚本平权协作的地基。

### 2. 用结构化字段做质量门（.when）

`.when` 直接读上游结构化字段（引擎内求值，不进 shell）。字段不达标 → 该实现不可用；全部不可用 → 步骤失败（fail-closed，不静默放行）：

```
  .proc("gate")
    .plan(g -> run("./publish.sh").when(@summarize.score >= 80))
```

### 3. 把不可信输出关进笼子（.trust）

`run()`/`sh()` 命令体里引用 `@ref` 必须显式声明 `.trust(@gen)`——LLM 输出含单引号会炸 shell 结构，这道闸强制你承认每一次注入，check 期带行号报错：

```
  .proc("build")
    .plan(r -> run("make @gen.target")).trust(@gen)
    .needs(@gen)
```

这不是外挂提示词，是把 shell 注入边界收进 DSL 语义：编译期检查引用范围，运行期只放行被点名的字段。

## 把脚本接进来

### 1. `script`：用契约头直接接入

`script` 是给任意语言准备的“薄接入层”：你先写一个普通脚本，再在文件顶部加一段 `ductile:` 契约头。它不强制绑定某个语言，也不要求你改造成某个框架的类；核心要求只有两点：**契约头存在且能通过静态检查，脚本自己正确实现输入输出协议**。

以文件 `my_tool.py` 为例，先注册：

```bash
ductile script attach my_tool.py
ductile script show my_tool      # 打印契约卡（AI 读这个，不读你的源码）
ductile script doctor            # 检查已注册脚本的契约文件是否还在
```

契约头长这样（声明参数/输出/副作用，引擎校验调用）：

```python
# ductile: v1
# name: word_stats
# desc: 统计词频
# lang: python
# params: text(str, required), n(int, default=10)
# output: words(int)
# pure: true
```

参数通过环境变量注入（`DUCTILE_ARG_<NAME>` 承载契约声明的每个参数，如 `DUCTILE_ARG_TEXT`；另有 `DUCTILE_TOPIC` 传 run 的 topic 实参——两者都不走 argv）；机器可读输出在 stdout 末尾打印协议块（每个 `output` 声明的字段都要给）：

```python
print("##DSL_RESULT")
print(f"words={n_words}")
print("##DSL_END")
```

打出这三行，引擎就能读到 `words` 字段，其他步骤用 `@步骤名.words` 引用。所谓"薄"，只是指**不需要你再写一层适配器框架**；契约头存在且通过静态检查、脚本按协议读写，这两点是硬要求。调用方写 `script(word_stats, text="{topic}")`。

### 2. `hyper`：把同一套拓扑换一个引擎

`.hyper` 用“顶点 + 平行边”描述一个可以有多条路径的图。在 `ductile import` 收进来的 `.pipeline` 或 `.hyper` 目录里，结构上的 key 和语义上的“死路”不是一回事；`.hyper` 更适合把“每个顶点各自有备选路径”直接写出来，生成管线时也天然支持这种拓扑复用。

`ductile import` 收进来的 `.pipeline` 或 `.hyper` 会进入统一的图注册表与显式目录，而不是只藏在当前目录里；Ductile 用这一套注册表做相似结构匹配，当前这些目录显式承担了“结构 key”以及“语义上的死路”标记的作用。你可以用 `ductile hyper similar` 直接查看当前已登记结构中，与给定 `.hyper` 最相似的那些图及其差异。Ductile 不要求把“相似”简化成手写的 key 或特例式硬编码，而是用结构上的相似度——而不是语义标签——来决定复用哪一段路径。

### 3. `tui`：在同一份数据上看全局

```bash
ductile tui
```

TUI 是纯读侧，四类视图分别解决不同问题：

- **STATUS**：一眼看全局：库总数、各库状态、重要认知标记。
- **DATA**：看执行与事故：每次运行的状态、哪一步出错、走了哪条路径。
- **BLUEPRINT**：从注册表里挑一个管线结构，查看它的依赖、门禁、信任边界和可能的环。
- **ISOMORPH**：像看相似结构报告一样，看某个管线与当前图里其他管线的对应关系和差异。

TUI 不执行也不修改管线，所有内容都从同一份 SQLite 库里读出来；因此“选一个结构”和“看一次执行”用的是同一套 key，不用在不同文件之间对照。

### 4. `explore`：出问题之后回看证据

```bash
ductile explore fallback.pipeline --budget 20 --attempts 8
```

`ductile explore` 按“出题 → 沙箱 → 确定性裁判 → 报告”的闭环工作：它会把一条管线拆成可独立评估的步骤，在受控沙箱里跑，再把每一次尝试的输入、输出、路径和判定固化下来。回看用 `ductile explore --report <报告id>`，报告 ID 就是你分享或存档那次探索的最小单位。

它不只是一个“重试”按钮，而是围绕四条评估闭环：

- **CANARY**：在把一条新路径推向生产前，先小流量验证它能不能走通。
- **INCIDENT**：在真实事故发生后，把事故的输入、决策和结果整理成一次可复盘的探索。
- **L4**：结构化评估层：让一条管线的“形状”本身可被比较，而不是只能靠人肉看日志。

`explore` 与 `canary`、`incident`、`l4` 共用同一个闭环：出题、运行、判定、形成可回看的报告；如果一条路径已经打开 incident，就不会继续替它跑 canary、更不会替它归档。

### 5. `patch`：把每次改动记在库里

```bash
ductile patch research search web enabled false
ductile patch clear
ductile db-stats
```

第一条把 `research.search.web` 临时关掉；`ductile patch clear` 把当前进程里所有临时 patch 撤掉；`ductile db-stats` 则看这次改动的统计入口。每个 patch 都有自己的来源标记：人工改的叫 `manual`，模型改的叫 `model`；这些来源都存在同一份 SQLite 历史里。`.plan` 用这份历史决定下次走哪条路，所以它不是“加了个监控面板”，而是执行路径本身的一部分。

`ductile toon` 是 TOON 的 closed parser 裁判口：它从 stdin 读入一段 TOON 文本，再把结果做一次 canonical 重编码后输出。payload 契约是"每条 `key: value` 冒号后恰好一个空格"；格式对不对与语义对不对拆开评估，解析器不悄悄接受"看起来差不多"的写法。

## 高级／按需：治理账本与决策记忆

新手可以先整段跳过这一节。下面的命令都需要 0.23.0 源码构建出来的二进制，完整参数面见 [SPEC.md](SPEC.md) 的 §2.1。

先在独立临时目录里跑通一条链式账本（以下命令均已实测通过）：

```bash
(
  set -eu
  DEMO_DIR="$(mktemp -d /tmp/ductile-readme.XXXXXX)"
  cd "$DEMO_DIR"

  # ① 生成 ed25519 密钥对（首参是路径占位；产物在 --out 目录）
  ductile mlv x.ledger keygen --out "$DEMO_DIR/keys"

  ROOT_KEY="$(find "$DEMO_DIR/keys" -type f -name '*.secret' -print -quit)"
  test -n "$ROOT_KEY"

  # ② 建 v2 链（v2 强制 ed25519）
  ductile ledger create app.ledger --root-key "$ROOT_KEY"

  # ③ 写一条决策记录：λ=0.5 τ=8 闭集 {A,B} 证据 A=0.4 B=0.6，上一轮 verified
  ductile attractor decide app.ledger cell 0 0.5 8.0 "A,B" "A=0.4,B=0.6" verified \
    --signing-key "$ROOT_KEY"

  # ④ 记忆投影 + 全量校验
  ductile attractor show app.ledger cell     # → runs / n / m / a / lambda / tau
  ductile ledger verify app.ledger           # → OK ledger verify format=v2(toon) …
)
```

这里用 `find` 找到 `keygen` 实际生成的 key ID，而不是假设某个固定文件名。生成的私钥只用于本地演示，不要提交到版本库。

### 写入口与只读验证

- `ledger create`：建立 append-only 的 v2 Ed25519 哈希链，是日常生产的直接写入口。
- `ledger verify`：只读校验链上的哈希和签名，不修改内容。
- `ledger convert`：读取旧链并产出一条新的 v2 链；它不向旧链追加，但同样是新链的产出路径。
- `mlv`：保留 `keygen`、`verify`、`status` 等残余接口（keygen=钥工具不写账本），不承担生产写；写动词已移除。

### `attractor`：从账本重放出决策记忆

决策记忆不是另建一份数据库，而是从账本事件重放得到。对候选 \(X\)，评分可以写成：

```text
q_X = g_X · (1 + λ · m · 𝟙(a = X))
m = e^(−n/τ),    λ ∈ [0, 0.5]
```

`a` 是当前记忆中的动作，`n` 是它在已验证历史里出现的次数；因此频繁被验证的动作会在后续决策中获得有限加成，但加成被 `λ` 的硬上限 `0.5` 限住。

只有 `verified` 事件会改变记忆状态；所有状态都可由账本重放，因此不需要独立维护一份“记忆文件”。如果两个候选评分相同，结果就是 `no-Decision`，不会偷偷打破平票。

`ductile attractor show app.ledger cell` 会打印 `runs`、`n`、`m`、`a`、`lambda`、`tau`；纯核 benchmark 中决策记忆 p99=0.0071ms（CLI 端到端另有 ~113ms 的整链 Ed25519 验签成本，属 append-only 语义而非决策计算）。

### 两条已知粗糙边缘

- `ductile attractor --help` 裸调用会 panic；这是已知的参数面问题，参数面以 SPEC 为准，不要把这次 panic 当成正常的帮助输出。
- `ductile mlv rotate` 的错误信息提到 `ledger append`，但实际上没有这个 verb；当前 ledger verbs 是 `create|verify|convert`。

## 五家对比：同一个问题，五种做法

下面把同一个任务——把一段非结构化文本变成结构化字段——放到五家框架的同一张表上比较。如果你想自己跑，可以先执行脚本套件（全部在离线沙箱里跑草图级别对比，不依赖各家云端服务）：

```bash
python benches/competitors/run_compare.py
ductile run examples/scripts/unstructured-extract.pipeline
```

| 能力 | LangGraph | AutoGen | CrewAI | Prefect／Airflow | Ductile |
| --- | --- | --- | --- | --- | --- |
| 核心范式 | 有状态图编排 | 多 agent 对话编排 | role／task 协作 | 以工作流或任务调度为中心 | 声明式管线 DSL |
| 失败换路 | 条件边或节点内编排 | 对话与工具编排 | task／role 内编排 | 重试与分支任务配置 | `.plan(a, b)` 由引擎换路 |
| 质量门 | 条件边或 reviewer 自定义 | 对话流程与 reviewer 自定义 | task／role 流程自建 | 依赖任务与检查配置 | `.when` 与结构化 `judge` |
| 不可信输出信任 | 应用层处理 | 应用层处理 | 应用层处理 | 应用层处理 | `.trust` 是 shell 注入闸 |
| 脚本契约 | tool 封装 | tool／function 封装 | task 封装 | 任务代码封装 | 契约头直接接入，仍须实现契约与协议 |
| 执行历史 | checkpoint 与框架记录 | 框架运行记录 | 框架运行记录 | 调度元数据与审计日志 | 本地 SQLite 单库 |
| 认知与事故 | 应用与监控层自建 | 应用与监控层自建 | 应用与监控层自建 | 调度与监控生态 | `incident`、`canary`、`l4` |
| 可审计账本 | 日志／checkpoint，非同一签名语义 | 日志，非同一签名语义 | 运行记录，非同一签名语义 | 元数据／审计日志 | v2 Ed25519 签名哈希链 |
| 本地可视化 | LangSmith／生态 UI | AutoGen Studio／生态 UI | 生态 UI | 各自开源基线（未评估托管版） | 内置 TUI |

### 公道与公平性说明

- 表中的离线草图只回答“同一输入能否变成同一类结构化输出”，不是各家产品的性能排名。
- 五家各自都有本文没展开的能力：checkpoint、调度、对话、插件、部署与生态都可能更成熟；Ductile 并不声称自己在这些维度全面领先。
- Ductile 选择的是另一条取舍：把失败换路、质量门、脚本契约、信任边界、本地历史和可审计账本放进同一个文件与同一套引擎，而不是让应用层再拼一遍。
- 可视化一行只比较这些项目的开源基线与生态入口；Prefect／Airflow 的企业或托管生态可能提供不同的 UI，不应把本表读成对所有发行方式的判断。

## 架构与文档入口

实现按八层组织：`interface`、`kernel`（约 12k 行）、`core`、`L4`、`L3`、`L2`、`L1`、`L0`；其中 `kernel` 采用平行生长策略，把决策内核与上层管线语义分开演化。

全量测试：

```bash
cargo test
```

当前测试套件是 718 个，包含 lib 与 integration tests，且全绿。它覆盖的不是一个“happy path demo”，而是把解析、编译、运行时换路、门禁、账本与回放都当成可测试的边界。

进一步阅读：

- [SPEC.md](SPEC.md)：完整语法、参数面与内核行为。
- [examples/](examples)：可运行的 `.pipeline`、`.hyper`、脚本与治理示例。
- [benches/](benches)：草图对比、决策记忆基准与纯核性能入口。

## 设计理念

> 你只声明要做什么，引擎负责让它做得更可靠。

- 管线文件就是配置：步骤、依赖、门禁与交付物都在一个地方。
- 失败换路不靠 `if/else` 堆叠，而靠声明式 `.plan` 把备选路径交给引擎。
- 质量门、信任边界与注入防护属于语义本身，不靠应用层自觉。
- 本地历史把运行记录变成决策输入；每条 AI 修改都可溯源、可回滚，制度兜底不靠自觉。

把流程写进文件，把可靠性交给引擎；把失败路径与审计依据留在同一份可读文档里。

## License

MIT

# Ductile

> 把多步工作写成一个文件。一条命令跑完。
> 某步失败时，引擎自动尝试声明的备选路径。
> 每次执行都留痕。可解释、可审计、可复用。

**版本**：0.23.0。Cargo、pyproject、PyPI 三源同版。`ledger`、`attractor`、`toon` 命令组均已发布。

初次阅读：装好、写一个 `.pipeline` 文件、用一次 `.plan()`。账本与决策记忆在高级章节，可跳过。

## 这是什么

Ductile 是一个声明式管线 DSL，用 Rust 编写。你声明"做什么"，引擎负责检查、调度、执行、换路、留痕。

- **一个文件描述整件工作**：步骤、依赖、门禁、交付物都写进 `.pipeline`。
- **失败自动换路**：一个步骤声明多个实现。失败后，引擎按声明序尝试下一条。
- **结构化协作**：AI、命令、脚本都产出结构化字段。下游按字段引用。
- **执行即留痕**：历史存入本地 SQLite 单库。供选路、排障、TUI 使用。

`.pipeline` 既是配置，也是可版本管理的流程文档。DSL 内置 **21 个动词**：`run`、`write`、`llm`、`script`、`mcp` 等。

## 首次上手（约 5 分钟，不含构建）

### 1. 安装与 PATH

前置：Linux 或 macOS。Rust 工具链。首次构建约数分钟。

```bash
git clone https://github.com/lsmind/ductile.git
cd ductile
cargo build --release
```

产物在 `target/release/ductile`。建软链后，任意目录都能调用：

```bash
mkdir -p "$HOME/.local/bin"
ln -sfn "$PWD/target/release/ductile" "$HOME/.local/bin/ductile"
export PATH="$HOME/.local/bin:$PATH"
command -v ductile
```

`$HOME/.local/bin` 通常已在默认 PATH。若不在，把 export 行写进 shell 配置：bash 用 `~/.bashrc`，zsh 用 `~/.zshrc`。

**版本核验**：跑 `ductile --version`。0.23.0 输出应含构建号；`--help` 应列出 `ledger`、`attractor`、`toon` 命令组。

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

`Pipeline(...)` 声明管线。`.proc(...)` 声明步骤。`@greet` 引用上一步输出。`.deliver(...)` 声明交付物。

```bash
ductile check hello.pipeline
ductile run hello.pipeline
cat /tmp/hello.txt
```

第三条命令输出 `hello ductile`。验证后删除临时文件：`rm /tmp/hello.txt`。

### 3. 加上备选路径

`.plan(a, b)` 给一个步骤声明多个实现。引擎先试 a。a 失败，自动试 b。不写条件判断。

新建 `fallback.pipeline`：

```text
Pipeline("fallback_demo", "有备选路径的管线")

  .proc("fetch")
    .plan(
      a -> run("exit 1").desc("故意失败"),
      b -> run("echo data from plan B").desc("备选方案")
    )

  .proc("deliver")
    .deliver(@fetch)
```

检查并执行：

```bash
ductile check fallback.pipeline
ductile run fallback.pipeline
```

a 退出码 1。引擎换 b。输出：`fetch => data from plan B`。任一实现成功，步骤即成功。

## 常用命令

| 命令 | 用途 |
| --- | --- |
| `ductile check <文件>.pipeline` | 静态检查，不执行 |
| `ductile run <文件>.pipeline` | 执行管线 |
| `ductile parse <文件>.pipeline` | 解析并输出结构 |
| `ductile graph <文件>.pipeline` | 导出依赖与拓扑 |
| `ductile db-stats` | 查看内核 DB 统计 |

日常执行用 `run`。提交前或 CI 里放 `check`。

## 让 AI 参与

### 1. 声明角色与结构化输出（llm + schema）

AI 是一种步骤，与命令平权。角色、提示词、输出字段定义在 `config.toml` 的 `[agents.x]`。管线内用 `schema` 强制结构化输出。下游用 `@步骤.字段` 引用：

```
  .proc("summarize", llm(analyst, prompt="把 {topic} 摘要", schema="title str, score int"))
```

字段格式崩了，不放行，不污染下游。这是 AI 与脚本平权协作的地基。

### 2. 用结构化字段做质量门（.when）

`.when` 读上游结构化字段。求值在引擎内，不进 shell。字段不达标，该实现不可用。全部不可用，步骤失败（fail-closed）：

```
  .proc("gate")
    .plan(g -> run("./publish.sh").when(@summarize.score >= 80))
```

### 3. 把不可信输出关进笼子（.trust）

`run()`/`sh()` 的命令体引用 `@ref` 时，必须声明 `.trust(@gen)`。LLM 输出含单引号会破坏 shell 结构。这道闸强制你显式承认每次注入。违规时 `check` 带行号报错：

```
  .proc("build")
    .plan(r -> run("make @gen.target")).trust(@gen)
    .needs(@gen)
```

注入边界收进 DSL 语义。编译期查引用范围。运行期只放行被点名的字段。

## 把脚本接进来

### 1. `script`：用契约头直接接入

`script` 是任意语言的薄接入层。先写普通脚本。再在文件顶部加 `ductile:` 契约头。不绑定语言，不改造成框架类。两条硬要求：契约头存在且过静态检查；脚本按协议读写。

以 `my_tool.py` 为例，注册：

```bash
ductile script attach my_tool.py
ductile script show my_tool      # 打印契约卡。AI 读这个，不读源码。
ductile script doctor            # 检查契约文件是否仍在
```

契约头声明参数、输出、副作用。引擎据此校验调用：

```python
# ductile: v1
# name: word_stats
# desc: 统计词频
# lang: python
# params: text(str, required), n(int, default=10)
# output: words(int)
# pure: true
```

参数经环境变量注入：每个参数一个 `DUCTILE_ARG_<NAME>`，如 `DUCTILE_ARG_TEXT`；`DUCTILE_TOPIC` 传 run 的 topic 实参。不走 argv。机器可读输出在 stdout 末尾打印协议块。每个 `output` 声明的字段都要给：

```python
print("##DSL_RESULT")
print(f"words={n_words}")
print("##DSL_END")
```

引擎从协议块读 `words`。其他步骤用 `@步骤名.words` 引用。"薄"指不用写适配器框架。契约与协议仍是硬要求。调用方写 `script(word_stats, text="{topic}")`。

### 2. `hyper`：拓扑复用

`.hyper` 用"顶点 + 平行边"描述多路径图。每个顶点各带备选路径。`ductile import` 收编 `.pipeline` 与 `.hyper`，进统一图注册表。注册表做相似结构匹配。用 `ductile hyper similar` 查看与给定图最相似的已登记结构及差异。复用判定靠结构相似度，不靠语义标签，不靠手写 key。

### 3. `tui`：终端看全局

```bash
ductile tui
```

TUI 是纯读侧。四类视图，各答一问：

- **STATUS**：全局状态。库总数、各库状态、认知标记。
- **DATA**：执行与事故。每次运行的状态、出错步骤、实际路径。
- **BLUEPRINT**：结构。依赖、门禁、信任边界、环。
- **ISOMORPH**：相似结构。管线间对应关系与差异。

TUI 不执行、不修改。全部数据读自同一份 SQLite 库。"选一个结构"与"看一次执行"用同一套 key。

### 4. `explore`：事后回看证据

```bash
ductile explore fallback.pipeline --budget 20 --attempts 8
```

`explore` 走四步闭环：出题、沙箱执行、确定性裁判、固化报告。回看用 `ductile explore --report <报告id>`。报告 ID 是分享与存档的最小单位。

同一闭环支撑三类评估：

- **CANARY**：新路径推生产前，小流量验证。
- **INCIDENT**：真实事故的输入、决策、结果，整理成可复盘记录。
- **L4**：管线"形状"可结构化比较，不靠人肉看日志。

一条路径已开 incident，就不再跑 canary，不再重复归档。

### 5. `patch`：改动记账

```bash
ductile patch research search web enabled false
ductile patch clear
ductile db-stats
```

第一条临时关闭 `research.search.web`。`patch clear` 撤销当前进程全部临时 patch。每条 patch 带来源标记：人工=`manual`，模型=`model`。来源存入同一份 SQLite 历史。`.plan` 用这份历史决定下次走哪条路。这不是监控面板，是执行路径的一部分。

### 6. `toon`：封闭解析裁判

`ductile toon` 从 stdin 读 TOON 文本。解析后做 canonical 重编码输出。payload 契约：每条 `key: value` 冒号后恰一空格。格式对错与语义对错分开评估。解析器不接受"看起来差不多"的写法。

## 高级／按需：治理账本与决策记忆

新手可跳过本节。完整参数面见 [SPEC.md](SPEC.md) §2.1。以下命令均已实测。

在临时目录跑通一条账本链：

```bash
(
  set -eu
  DEMO_DIR="$(mktemp -d /tmp/ductile-readme.XXXXXX)"
  cd "$DEMO_DIR"

  # ① 生成 ed25519 密钥对。首参为路径占位，产物在 --out 目录。
  ductile mlv x.ledger keygen --out "$DEMO_DIR/keys"

  ROOT_KEY="$(find "$DEMO_DIR/keys" -type f -name '*.secret' -print -quit)"
  test -n "$ROOT_KEY"

  # ② 建 v2 链。v2 强制 ed25519。
  ductile ledger create app.ledger --root-key "$ROOT_KEY"

  # ③ 写一条决策记录。λ=0.5，τ=8，闭集 {A,B}，证据 A=0.4 B=0.6，上一轮 verified。
  ductile attractor decide app.ledger cell 0 0.5 8.0 "A,B" "A=0.4,B=0.6" verified \
    --signing-key "$ROOT_KEY"

  # ④ 记忆投影 + 全量校验
  ductile attractor show app.ledger cell     # → runs / n / m / a / lambda / tau
  ductile ledger verify app.ledger           # → OK ledger verify format=v2(toon) …
)
```

`find` 定位 keygen 实际生成的密钥文件。不假设固定文件名。私钥仅用于本地演示。勿提交版本库。

### 写入口与只读验证

- `ledger create`：建 append-only v2 Ed25519 哈希链。日常生产写入口。
- `ledger verify`：只读校验哈希与签名。不改内容。
- `ledger convert`：读旧链，产新 v2 链。不向旧链追加。
- `mlv`：残余接口 `keygen`/`verify`/`status`。keygen 是钥工具，不写账本。写动词已移除。

### `attractor`：从账本重放决策记忆

决策记忆不另建数据库。它从账本事件重放导出。候选 \\(X\\) 的评分：

```text
q_X = g_X · (1 + λ · m · 𝟙(a = X))
m = e^(−n/τ),    λ ∈ [0, 0.5]
```

`a` 是记忆中的当前动作。`n` 是它在已验证历史中的次数。频繁被验证的动作获得有限加成。加成被 λ 硬上限 0.5 封死。

仅 `verified` 事件改变记忆状态。全部状态可由账本重放，无需独立"记忆文件"。两候选评分相同，输出 `no-Decision`。不偷偷破平。

`attractor show` 打印 `runs`、`n`、`m`、`a`、`lambda`、`tau`。纯核基准 p99=0.0071ms。CLI 端到端另有 ~113ms 整链 Ed25519 验签成本——属 append-only 语义，不属决策计算。

## 五家对比：同一个问题，五种做法

比较任务：同一段非结构化文本，转成结构化字段。要自己跑，执行：

```bash
python benches/competitors/run_compare.py
ductile run examples/scripts/unstructured-extract.pipeline
```

两者均在离线沙箱跑草图级对比。不依赖各家云端服务。

| 能力 | LangGraph | AutoGen | CrewAI | Prefect／Airflow | Ductile |
| --- | --- | --- | --- | --- | --- |
| 核心范式 | 有状态图编排 | 多 agent 对话编排 | role／task 协作 | 工作流与任务调度 | 声明式管线 DSL |
| 失败换路 | 条件边或节点内编排 | 对话与工具编排 | task／role 内编排 | 重试与分支任务配置 | `.plan(a, b)` 引擎换路 |
| 质量门 | 条件边或 reviewer 自定义 | 对话流程与 reviewer 自定义 | task／role 流程自建 | 依赖任务与检查配置 | `.when` 与结构化 `judge` |
| 不可信输出信任 | 应用层处理 | 应用层处理 | 应用层处理 | 应用层处理 | `.trust` 注入闸 |
| 脚本契约 | tool 封装 | tool／function 封装 | task 封装 | 任务代码封装 | 契约头接入，仍须实现契约与协议 |
| 执行历史 | checkpoint 与框架记录 | 框架运行记录 | 框架运行记录 | 调度元数据与审计日志 | 本地 SQLite 单库 |
| 认知与事故 | 应用与监控层自建 | 应用与监控层自建 | 应用与监控层自建 | 调度与监控生态 | `incident`、`canary`、`l4` |
| 可审计账本 | 日志／checkpoint，非同一签名语义 | 日志，非同一签名语义 | 运行记录，非同一签名语义 | 元数据／审计日志 | v2 Ed25519 签名哈希链 |
| 本地可视化 | LangSmith／生态 UI | AutoGen Studio／生态 UI | 生态 UI | 各自开源基线（未评估托管版） | 内置 TUI |

### 公道与公平性说明

- 离线草图只回答"同一输入能否变成同类结构化输出"。不是性能排名。
- 五家各有力本文未展开：checkpoint、调度、对话、插件、部署、生态都可能更成熟。Ductile 不声称全面领先。
- Ductile 的取舍：换路、质量门、脚本契约、信任边界、本地历史、可审计账本，放进同一个文件与同一套引擎。不让应用层再拼一遍。
- 可视化行只比较开源基线。Prefect／Airflow 的托管生态提供不同 UI。勿把本表读成对所有发行方式的判断。

## 架构与文档入口

实现分八层：`interface`、`kernel`（约 12k 行）、`core`、`L4`、`L3`、`L2`、`L1`、`L0`。kernel 平行生长：决策内核与上层管线语义分开演化。

全量测试：

```bash
cargo test
```

当前 718 个测试，lib 加集成，全绿。覆盖解析、编译、运行时换路、门禁、账本、回放。不是 happy path demo。

进一步阅读：

- [SPEC.md](SPEC.md)：完整语法、参数面、内核行为。
- [examples/](examples)：可运行的 `.pipeline`、`.hyper`、脚本、治理示例。
- [benches/](benches)：草图对比、决策记忆基准、纯核性能入口。

## 设计理念

> 你只声明做什么。引擎负责让它可靠。

- 管线文件即配置：步骤、依赖、门禁、交付物在一处。
- 换路不靠 `if/else` 堆叠。声明 `.plan`，引擎选路。
- 质量门、信任边界、注入防护属语义本身。不靠应用层自觉。
- 本地历史把运行记录变成决策输入。每条 AI 修改可溯源、可回滚。制度兜底，不靠自觉。

把流程写进文件。把可靠性交给引擎。失败路径与审计依据留在同一份可读文档里。

## License

MIT

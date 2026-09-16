# Ductile

> 把一件需要好几步才能完成的工作，写成一个文件、一条命令跑完。
> 中间某步失败了？它会自己换备用方案，不用你写 if/else。

---

## 这是什么

假设你要让 AI（或脚本）帮你干一件分好几步的活，比如：

1. 先搜资料
2. 再让 AI 写成摘要
3. 最后存成文件

传统做法是写一个 Python 脚本，一步步串起来。但很快你会遇到一堆烦心事：

- 某一步失败了，整个脚本崩掉，你得手动重跑
- 想换一种搜索方式，得改代码
- AI 输出格式不对，下游解析报错
- 过两周回来看，忘了哪步是哪步

**Ductile 的答案：把要做的事写进一个文本文件（叫"管线"），交给引擎执行。**

- 某步有多个备选做法？写上就行，失败自动换下一个
- 想看每步的输入输出？引擎全部记录在案
- 文件本身就是文档，谁都能看懂整条流程

## 5 分钟上手

### 第 1 步：安装

```bash
git clone https://github.com/lsmind/ductile.git
cd ductile
cargo build --release
```

装好后，命令叫 `ductile`（在本仓库就是 `target/release/ductile`）。

### 第 2 步：写第一个文件

新建一个文本文件 `hello.pipeline`，内容如下：

```
Pipeline("hello", "我的第一条管线")

  // 第一步：执行一条命令，输出一句话
  .proc("greet", run("echo hello ductile"))

  // 第二步：把上一步的输出存进文件（@greet 就是"上一步的结果"）
  .proc("save", write(to="/tmp/hello.txt", content=@greet))

  // 收尾：声明"save 是这条管线的最终产物"
  .proc("deliver")
    .deliver(@save)
```

它读起来就像一份清单：

| 你写的 | 意思是 |
|---|---|
| `Pipeline("hello", "说明")` | 这条管线叫 hello，一句话说明用途 |
| `.proc("greet", run(...))` | 一个步骤，名叫 greet，执行一条命令 |
| `@greet` | 引用 greet 步骤的输出 |
| `.deliver(@save)` | save 的产出就是最终交付物 |

### 第 3 步：跑起来

```bash
ductile run hello.pipeline
```

你会看到：

```
Pipeline executed successfully
  Procs completed: 2
    greet => hello ductile
    save => <file: /tmp/hello.txt>
```

打开 `/tmp/hello.txt`，内容就是 `hello ductile`。

### 第 4 步：体验"自动换备用方案"

新建 `fallback.pipeline`：

```
Pipeline("fallback_demo", "第一步失败了自动换备用方案")

  .proc("fetch")
    .plan(
      a -> run("exit 1").desc("故意失败"),
      b -> run("echo data from plan B").desc("备用方案")
    )

  .proc("deliver")
    .deliver(@fetch)
```

跑一下：

```bash
 ductile run fallback.pipeline t
```

输出里能看到 `trying: a` 失败后自动 `trying: b`，最终成功。**你没有写一行错误处理代码。**

## 常用命令

| 命令 | 干什么用 |
|---|---|
| `ductile check 文件` | 只检查文件写得对不对，不执行 |
| `ductile run 文件` | 检查 + 执行 |
| `ductile graph 文件` | 画出步骤之间的依赖关系 |
| `ductile parse 文件` | 显示引擎怎么理解你的文件 |

先 `check` 后 `run` 是好习惯——错误在执行前就被拦下。

## 功能清单

### 备选路径与自动降级

一个步骤写多个实现，失败自动换下一个，重试与退避由引擎处理：

```
  .proc("fetch")
    .plan(
      a -> run("./fetch_v1.sh"),
      b -> run("./fetch_v2.sh").desc("备用方案")
    )
```

路径排序按**历史成功率**学习（窗口 20 次，失败率 >10% 指数降权，连续 3 败拉黑），跑得越久选得越准。

### 条件执行（.when）

条件在引擎内求值（读上游结构化字段，不进 shell）：

```
  .proc("gate")
    .plan(g -> run("./build.sh").when(@gen.ok == 1))
```

条件不满足 → 该实现不可用；全部不可用 → 步骤失败（fail-closed，不静默放行）。

### 结构化输出（llm + schema）

AI 步骤强制吐 `字段 类型` 对，引擎解析成结构化结果，下游用 `@步骤.字段` 引用：

```
  .proc("summarize", llm(analyst, prompt="把 {topic} 摘要", schema="title str, score int"))
  .proc("gate")
    .plan(g -> run("echo pass").when(@summarize.score >= 80))
```

格式崩了不放行，不污染下游。这是管线里 AI 与脚本平权协作的地基。

### 依赖与信任声明（.needs / .trust）

- `.needs(@up)`：把上游产出喂进本步骤 AI 的上下文
- `.trust(@up)`：**安全闸**——run()/sh() 命令体里引用 `@up` 必须点名，否则检查期直接报错（带行号）。上游文本含引号会炸 shell，这道闸强制你显式承认每一次注入

```
  .proc("build")
    .plan(r -> run("make @gen.target")).trust(@gen)
    .needs(@gen)
```

### 多模型档位

同一角色配置阶梯（本地小模型 → 云端大模型），失败自动升档：

```toml
[models.light]
model = "qwen3.8:9b"
[models.high]
model = "glm-4.7"

[agents.analyst]
system  = "……"
tiers   = "light,high"
```

换模型改配置，不改管线。

### 脚本契约（script attach）

任意语言脚本注册成带契约的步骤——头部注释声明参数/输出/副作用，引擎校验调用：

```bash
ductile script attach my_tool.py
ductile script show my_tool      # 契约卡：AI 读这个，不读你的源码
ductile script doctor            # 检查契约文件是否还在
```

调用方 `script(my_tool, text="{topic}")`，传参走环境变量，输出走结构化协议。纯函数+幂等+并发安全的脚本自动获得 CSE/并行资格。

### 拓扑复用（hyper）

公共结构提炼成 `.hyper` 文件（vertex + hedge），一条命令生成新管线；写新图前 `hyper similar` 查重。

`similar` 的语料是 db 注册表（`ductile import` 收的 `.pipeline`/`.hyper` 都入册）加上显式目录参数——import 过的管线无论放在哪个目录都能被查到；结构键每次从文件现算，注册表里的死路径会跳过并标注。

### TUI 操作台（v0.20）

```bash
ductile tui
```

终端里的四视图操作台（蓝金暗色）：

- **STATUS**——库计数、认知层旗标（l4 阶段/incidents/降级管线/canary 通过率）
- **DATA**——最近执行记录浏览器 + 问题单列表
- **BLUEPRINT**——左侧从 db 注册表选管线（`/` 过滤、死路径标 ✗），右侧渲染成节点蓝图：deliver 节点金框，依赖/门禁/信任/循环四种边分开画
- **ISOMORPH**——选中管线后加载 `hyper similar` 报告 + structure_key，判断"这个新图是不是重复造轮子"

纯读侧：不写库、不执行管线。键位 `1-4` 切视图、`j k` 光标、`Enter` 选中、`q` 退出。

### 自动探索（explore）

```bash
ductile explore 管线.pipeline "要验证的能力"    # 出题→沙箱跑→确定性裁判
ductile explore --report <报告id>              # 回看冻结报告
```

发现的问题固化成结构化问题单（incidents），修复后关闭。

### 认知回传（canary / incident / l4）

- `canary`：归档"已知好输入"，回归时先跑金丝雀判"是上游投毒还是本地问题"
- `incident`：问题单生命周期；open 期间自动禁止归档金丝雀（矛盾态保护）
- `l4`：端到端复核记录，攒够人工标签后从"只记录"升格"会拦截"
- `archive`：数据库快照（保留 10 份）——危险操作前先拍一份

### 运行时热补丁（patch）

不改源文件临时禁用/调整某实现：`ductile patch research search web enabled false`，`patch clear` 一键还原。每条补丁记录出处（人手敲 / 哪个模型开的）。

## 与其他产品的对比

同一任务（非结构化文本 → 结构化字段）在五个框架下的形态，**可跑对照在 [`benches/competitors/`](benches/competitors/)**：

```bash
python benches/competitors/run_compare.py          # 四家离线草图 + 对比矩阵
ductile run examples/scripts/unstructured-extract.pipeline   # 同场景 ductile 版
```

| | LangGraph | AutoGen | CrewAI | Prefect/Airflow | **Ductile** |
|---|---|---|---|---|---|
| 范式 | 图状态机 + ReAct | 多 agent 会话 | 角色团队 | 批处理 DAG | 声明式多路径 |
| 失败换路 | 手写节点逻辑 | 对话轮里涌现 | role 内 try | 手写重试块 | **`.plan(主, 备)` 引擎内置** |
| 质量门 | 条件边手写 | reviewer agent 商量 | task 回调 | 无 | **judge proc + `.when` 结构化字段路由** |
| AI 输出安全 | 应用层自理 | 应用层自理 | 应用层自理 | 不涉及 | **`.trust` 引擎闸：上游文本进 shell 必须点名** |
| 脚本接入 | tool 封装代码 | tool 封装代码 | tool 封装代码 | task 封装代码 | **契约头注释即接口，零封装** |
| 执行历史 | 框架各表 | 框架各表 | 框架各表 | 元数据库 | **SQLite 单库，历史直接驱动路径选择** |
| 问题追踪 | 无内建 | 无内建 | 无内建 | 日志层 | **incident/canary/l4 认知层内建** |
| 本地可视化 | LangSmith 云服务 | AutoGen Studio | 无 | UI 付费版 | **`ductile tui` 终端四视图（纯读侧，零依赖）** |

**公平性说明**（怕误导，写清楚）：

- 对照脚本是无依赖**离线草图**（stub 数据，毫秒级），演示的是各家的**结构形态**，不是各家真实框架的性能——别拿 latency 数字当横评
- 上游框架的能力远不止此表（LangGraph 的 checkpoint、Prefect 的调度生态都是 Ductile 没有的）；这张表只回答"**失败换路 + 质量门 + 脚本契约**这三件事在各家手里长什么样"
- Ductile 的取舍：不做对话编排、不做定时调度生态——把"多路径声明、裁判分离、注入安全"做成引擎语义而非应用层惯例

## 接入你自己的工具

Ductile 不要求你重写工具。任何语言的脚本，只要在输出里多打印几行"标准格式"（我们叫协议），就能被当作一个步骤编排进来：

```python
# my_tool.py — 你的脚本只需在最后打印这几行
print("##DSL_RESULT")
print("count=42")
print("##DSL_END")
```

打印了这三行，引擎就能读到 `count=42` 这个结构化结果，其他步骤可以用 `@步骤名.count` 引用它。完整说明见 [SPEC.md](SPEC.md)。

## 让 AI 参与管线

你可以让 AI 处理其中某些步骤。AI 是管线里的一种"步骤"，和普通命令平起平坐：

```
  .proc("summarize", llm(prompt="把 {topic} 写成 100 字摘要"))
```

配置好 AI 的接入方式后（见 [SPEC.md](SPEC.md) 的说明），这行就能跑。AI 步骤失败同样自动换备用方案，输出同样被记录。

## 如果某步失败了

引擎把每次执行都记进一个本地数据库（SQLite 单文件）。你可以：

- 直接看执行日志（run 的输出）
- 用 `ductile db-stats` 查执行记录统计（哪个步骤老失败，一目了然）

失败的步骤会留下记录，同一问题反复出现时引擎会标记它。**你不需要装任何额外的监控系统。**

## 设计理念

**你只管声明"要做什么"，引擎负责"怎么做好"。**

- 多个备选方案、失败重试、错误传播——都是引擎内置行为，不是你的代码
- 执行记录、成功率统计——自动留存，不是你的代码
- 每条 AI 建议的修改都记录出处、可回滚——机器不可全信，制度兜底

一句话：**把流程写成文件，把可靠性交给引擎。**

## 想深入的话

- [SPEC.md](SPEC.md) —— 完整说明书（也可直接喂给 AI 助手让它帮你写管线）
- [examples/](examples/) —— 官方示例，每个都能直接跑
- 测试：`cargo test --lib`（476 项全过）

## License

MIT

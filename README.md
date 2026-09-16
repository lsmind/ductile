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

## 逐步升级：从"能跑"到"好用"

上面 5 分钟的内容只用了 Ductile 的一成功力。往下每一级都是**一个独立的功能**，
你可以在任何一级停下来——够用就好；也可以一路往上，把整条工作流变成
"一个文件 + 一条命令"。

### 第 1 级：条件执行 —— `.when`

某步只在条件满足时才跑（条件是引擎读上游的**结构化字段**，不是字符串比较）：

```
  .proc("audit")
    .plan(a -> run("./check.sh").when(@gen.ok == 1))
```

`@gen.ok` 是 gen 步骤输出的字段。条件不满足 → 这一步自动跳过，下游继续。

### 第 2 级：显式依赖与信任 —— `.needs` / `.trust`

- `.needs(@upstream)`：声明"这一步要参考上游的产出"——喂给 AI 步骤当上下文
- `.trust(@upstream)`：声明"我允许上游产出进入我的 shell 命令"——**安全闸**，
  没点名的引用会被引擎在检查期直接拦下（带行号报错），防注入

```
  .proc("gen", llm(prompt="...", schema="sid int, pass bool"))
  .proc("build")
    .plan(r -> run("make @gen.target")).trust(@gen)
    .needs(@gen)
```

### 第 3 级：AI 步骤 + 结构化输出 —— `llm` + `schema`

AI 参与管线的关键不是"能调 AI"，是**输出必须结构化**。`schema` 强制 AI
吐 `字段 类型` 对，引擎解析成结构化结果，下游用 `@步骤.字段` 引用——
格式崩了引擎不会放行，不会污染下游：

```
  .proc("summarize", llm(prompt="把 {topic} 摘要", schema="title str, score int"))
  .proc("gate")
    .plan(g -> run("echo pass").when(@summarize.score >= 80))
```

### 第 4 级：多模型档位 —— 一个 proc 多个 impl

同一个步骤写多个实现（本地小模型 / 云端大模型 / 纯脚本兜底），引擎按
**历史成功率**自动选——谁老成功用谁，失败自动降档换下一个：

```
  .proc("gen")
    .plan(
      local  -> llm(prompt="...", agent="qwen_local"),
      cloud  -> llm(prompt="...", agent="glm_cloud"),
      stub   -> run("./fallback.sh")
    )
```

跑得越久，选择越准（执行历史全部落库）。

### 第 5 级：脚本即 API —— `script attach`

把任意语言的脚本注册成带**契约**的步骤：脚本头部声明输入输出，引擎负责
校验和调用。你的脚本不用改一行重试/降级逻辑——那是引擎的事：

```bash
ductile script attach my_tool.py
ductile script show my_tool     # 看契约卡（AI 读这个，不读你的源码）
ductile script doctor           # 检查所有契约的文件还在不在
```

### 第 6 级：拓扑复用 —— `hyper`

多条管线长得很像？把公共结构提炼成 `.hyper` 拓扑文件，`ductile hyper build`
一条命令生成新管线。写新管线前 `ductile hyper similar` 先查有没有现成拓扑可抄。

### 第 7 级：自动探索 —— `explore`

不知道某条管线在陌生环境里行不行？让引擎自己出题、自己探、自己判：

```bash
ductile explore 管线.pipeline "要验证的能力"     # 出题→沙箱探针→确定性裁判
ductile explore --report <报告id>                 # 回看冻结的探索报告
```

发现的问题自动固化成 incidents（结构化问题单），修复后关闭，引擎记录全过程。

### 第 8 级：认知回传 —— canary / incident / l4

- `ductile canary`：把"已知好输入"存档，回归时先跑金丝雀，绿了再跑真的
- `ductile incident`：问题单生命周期（open → close），矛盾期间自动禁播金丝雀
- `ductile l4`：端到端 review 记录（谁改的、为什么、结果如何，全部留痕）

这一级是"引擎记住自己的经验"：跑过的坑不用再踩第二遍。

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

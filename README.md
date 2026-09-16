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
- [docs/](docs/) —— 各专题文档
- 测试：`cargo test --lib`（471 项全过）

## License

MIT

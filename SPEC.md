# Ductile DSL — API 规格文档（供 AI 调用者阅读）

> 本文档面向 AI agent / LLM 调用者。读完本文档后，你应能独立完成 Ductile 的安装、pipeline 编写、执行、调试和调优。

---

## 0. 快速开始

```bash
pip install ductile
```

验证安装：

```bash
ductile --help
```

如果 `ductile` 不可用，从源码编译：

```bash
git clone https://github.com/lsmind/ductile.git
cd ductile && cargo build --release
# 二进制：target/release/ductile，加入 PATH 或用全路径调用
```

---

## 1. 编写 .pipeline 文件

### 1.1 文件格式

纯文本，扩展名 `.pipeline`。`//` 开头为注释，空行忽略。

### 1.2 语法规则

```
Pipeline("name", "optional description")
  .proc("proc_name")
    .desc("optional description")
    .plan(
      impl_name -> body_function(args)
        .tags(#tag1, #tag2)
        .desc("optional")
        .retry(n=3)
        .when(mode == "deep")
        .disabled
        .stub
        .cost(latency=100, risk=0.1, tokens=500, money=0.01)
        .ensure(result => predicate, "error message"),
      fallback_name -> body_function(args)
        .tags(#tag3, #tag4)
    )
    .check(result => predicate, "error message")
    .foreach(source=@upstream_proc, var=item_name)
    .deliver(@upstream_proc)
```

### 1.3 语法约定

- Pipeline 必须以 `Pipeline("name")` 或 `Pipeline("name", "desc")` 开头
- 每层缩进 2 空格（仅人类可读性，解析器不依赖缩进）
- `.plan(...)` 内多条路径用逗号分隔
- 路径格式：`name -> body`（`->` 前是 impl 名，后面是函数调用文本）
- `.tags()` 只在 impl（叶子）上声明
- `.deliver()` 的 proc 不执行，仅标记终端输出
- 标签用 `#` 前缀，标签集是 BTreeSet（有序、去重）
- 依赖关系自动推导：body 中 `@proc_name` 引用即声明依赖

### 1.4 变量替换规则

| 语法 | 替换为 | 时机 |
|------|--------|------|
| `{topic}` | run 命令的 topic 参数 | 执行前 |
| `{hash(topic)}` | topic 的 djb2 hash | 执行前 |
| `{var_name}` | foreach 的当前 item | 每次 foreach 迭代 |
| `@proc_name` | 上游 proc 的完整输出文本 | 依赖完成后 |
| `@proc_name.field` | 上游 proc 的 DSL_RESULT 字段值 | 依赖完成后 |

未匹配的 `@name` 原样保留。未匹配的 `{name}` 原样保留。

### 1.5 内置谓词（check / ensure）

| 谓词 | 判断逻辑 |
|------|---------|
| `not_empty` | `text.len() > 0` |
| `has_results` / `has_content` / `has_summary` / `valid_output` | `text.len() > 20` |
| `has_items` | 非空行数 ≥ 2 |
| `no_error` | 不含 "ERROR" / "Error" / "error" |
| `has_date` | 含 2020-2030 范围内的年份字符串 |
| `has_citations` | 含 `[1]` 或 `[链接` 或 `来源` |
| `file_exists` / `has_keywords` | `text.len() > 10` |
| 其他 | 默认通过（pass） |

### 1.6 内置函数

| 函数 | body 文本格式 | 说明 |
|------|--------------|------|
| `web_search` | `web_search(query="...")` | 网页搜索 |
| `mcp_search` | `mcp_search(query="...", engine=zai)` | MCP 搜索 |
| `llm` | `llm(input=@prev, template="...", count=5)` | LLM 调用 |
| `write` | `write(to="path", content=@prev)` | 写文件 |
| `read` | `read(from="path")` | 读文件 |
| `run` | `run("shell command")` | 执行 shell 命令 |
| `sh` | `sh("shell command")` | run 的别名 |
| `merge` | `merge(@a, @b, dedup)` | 合并结果（可选去重） |
| 其他 | 原样返回 `<noop: func_name>` | 不报错 |

`run` / `sh` 的输出如果包含 `##DSL_RESULT` 块，自动解析为结构化数据（见第 7 节）。

---

## 2. CLI 命令参考

### 2.1 check

```bash
ductile check <file.pipeline>
```

- 输入：`.pipeline` 文件路径
- 动作：解析 + 类型检查
- 退出码：0 = 通过，1 = 有错误
- 输出：`Type check passed` 或错误列表

### 2.2 run

```bash
ductile run <file.pipeline> [topic] [--key=value ...]
```

- 输入：文件路径 + 可选 topic + 可选参数
- topic 格式：`--` 之前的所有词 join 为 topic
- 参数格式：`--` 之后的 `key=value` 对
- 动作：解析 + 检查 + 执行
- 副作用：
  - 自动导入 pipeline 到 SQLite
  - 执行记录写入 SQLite runs 表
  - 同构提示输出到 stdout
  - patch 应用日志输出到 stderr
- 退出码：0 = 成功，1 = 失败

**示例：**

```bash
ductile run research.pipeline "AI 安全" --mode=deep --lang=zh
```

topic = `"AI 安全"`，params = `{mode: "deep", lang: "zh"}`

### 2.3 parse

```bash
ductile parse <file.pipeline>
```

- 动作：仅解析，展示 AST 结构 + 同构提示
- 不执行，不写 SQLite

### 2.4 graph

```bash
ductile graph <file.pipeline>
```

- 动作：展示 E-graph 结构
- 输出：节点数、边数、并行组、关键路径

### 2.5 import

```bash
ductile import <dir>
ductile import <file.pipeline>
ductile import <dir1> <dir2> <file.pipeline>
```

- 动作：批量导入 `.pipeline` 文件到 SQLite
- 目录会递归扫描 `.pipeline` 文件

### 2.6 search

```bash
ductile search "query"
```

- 动作：在 proc 库中搜索（LIKE 匹配 tags + name + description）
- 输出：匹配的 proc 列表（名称、tags、描述、来源 pipeline、impl 数）

### 2.7 compose

```bash
ductile compose <name> <description> <#tag1,#tag2> <#tag3,#tag4> ...
```

- 动作：按 tag 链从 proc 库组装 pipeline
- 每步输出 top-3 候选 + 自动选第一个
- 结果保存到 compositions 表

**示例：**

```bash
ductile compose "translate" "翻译流水线" "#search,#web" "#llm,#parse" "#write,#file"
```

### 2.8 db-stats

```bash
ductile db-stats
```

- 输出：pipelines / procs / runs / compositions 计数 + 数据库路径

### 2.9 discover

```bash
ductile discover
ductile discover <file.pipeline>
```

- 动作：展示跨 pipeline 的同构 proc 组（tag 集合相同）
- 如果给 file 参数，先导入再发现

### 2.10 learn

```bash
ductile learn [dir]
```

- 无参数：扫描 `~/projects/ductile/pipelines/` + `experiments/`
- 有参数：扫描指定目录
- 动作：静态扫描 tag 序列，发现频率 ≥ 2 的重复模式
- 输出：重复模式 + 压缩率

### 2.11 patch

```bash
ductile patch <pipeline> <proc> <impl> <field> <value>
ductile patch list
ductile patch clear <pipeline>
```

- 动作：运行时覆盖节点属性，不改源文件
- 存储在 SQLite patches 表（UPSERT 语义）

**可 patch 字段：**

| field | value | 类型 |
|-------|-------|------|
| `enabled` | `true` / `false` | bool |
| `cost.latency` | 整数 | i64 |
| `cost.risk` | 浮点数 | f64 |
| `cost.tokens` | 整数 | i64 |
| `cost.money` | 浮点数 | f64 |
| `retry` | 整数 | usize |
| `stub` | `true` / `false` | bool |

**示例：**

```bash
ductile patch research search web enabled false
ductile patch research search web cost.latency 5000
ductile patch research summarize s1 retry 5
ductile patch research search mcp stub true
ductile patch list
ductile patch clear research
```

### 2.12 version

```bash
ductile version save <file.pipeline> "change description"
ductile version log <file.pipeline>
ductile version diff <file.pipeline> <v1> <v2>
```

- 存储在 `~/.local/share/ductile/versions/<pipeline_name>/`

---

## 3. 执行引擎行为

### 3.1 执行顺序

1. `apply_patches(pl)` — 从 SQLite 加载补丁，克隆并覆盖
2. `build_egraph(pl)` — 构建 DAG
3. `parallel_groups(eg)` — 拓扑分层
4. 逐层执行（层内串行）

### 3.2 路径选择算法

对每个 proc：

```
eligible = plan.filter(impl => when_condition_passes)
ranked = eligible.sort_by(score)

score(impl) = base_cost × (1 + penalty) + rd_surcharge
```

- `base_cost = 0.001×latency + 10×risk + 0.0001×tokens + 1×money`
- `penalty` 见下表
- `rd_surcharge = weights.rd × (Σest_loss / max(Σrate_tokens, 1))`（见 3.6）

### 3.3 滑动窗口惩罚

每个 impl 保留最近 20 次执行记录。

| 条件 | penalty 值 | 效果 |
|------|-----------|------|
| 无记录 | 0.0 | 原始排序 |
| 窗口 < 3 | 0.0 | 冷启动探索 |
| 失败率 ≤ 10% | 0.0 | 无惩罚 |
| 失败率 > 10% | `e^(7×rate) - 1` | 指数惩罚 |
| 连续失败 ≥ 3 | `∞` | 永久 BLOCKED |

**惩罚域与失真域分家（反双重计费）**：penalty 只管瞬时故障（可 retry 恢复的失败），est_loss 只管信息质量（重试也不会好的字段丢失）。同一次失败不会同时吃两种惩罚——est_loss 无结构化证据时恒为 0。

### 3.6 RD 附加费（率失真感知排序）

每次成功执行自动测量并落库：

- `rate_tokens` = 输出字符数 / 4（token 代理）
- `est_loss` = **v1 字段覆盖度**：上游字段集 `F_up`（body 中 `@dep` 引用的上游 DSL_RESULT 字段并集）与输出字段集 `F_out` 的保留度 `1 − |F_up ∩ F_out| / |F_up|`；**无结构化证据（上游无字段或输出无 DSL_RESULT）时 = 0.0，不用 check 二值兜底**

排序时（窗口内聚合）：

```
rd_surcharge = weights.rd × (Σest_loss / max(Σrate_tokens, 1))
```

- `weights.rd`（λ_rd）默认 `0.0` = 完全关闭（行为同 v4.0）；可在 `.plan()` 内 `weights(rd = N)` 设置
- 语义：同样 token 预算下单位信息损耗大的 impl 排后——"快但丢字段"不再无条件赢
- 冷启动（无历史记录）不惩罚

### 3.4 retry

`.retry(n=3)` 失败后等待 `2^(attempt+1)` 秒：2s → 4s → 8s。共尝试 n+1 次。

### 3.5 check 流程

impl 执行成功后，运行 proc 级 `.check()` + impl 级 `.ensure()`：

- 全部通过 → proc 成功
- 任一失败 → 当前 impl 标记 Fail → 尝试下一个 impl

---

## 4. SQLite schema

数据库路径：`~/.local/share/ductile/ductile.db`

```sql
CREATE TABLE pipelines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL,
    description TEXT DEFAULT '',
    source_file TEXT DEFAULT '',
    imported_at TEXT DEFAULT ''
);

CREATE TABLE procs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    pipeline_id INTEGER NOT NULL,
    description TEXT DEFAULT '',
    tags TEXT DEFAULT '',          -- 逗号分隔
    impl_count INTEGER DEFAULT 0,
    is_deliver INTEGER DEFAULT 0,
    FOREIGN KEY (pipeline_id) REFERENCES pipelines(id)
);

CREATE TABLE runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    proc_name TEXT NOT NULL,
    impl_name TEXT NOT NULL,
    pipeline TEXT DEFAULT '',
    status TEXT NOT NULL,          -- "Ok" 或 "Fail"
    latency_ms INTEGER DEFAULT 0,
    err_hash TEXT DEFAULT '',
    err_at TEXT DEFAULT '',
    rate_tokens INTEGER DEFAULT 0, -- RD: 输出字符数/4（token 代理）
    est_loss REAL DEFAULT 0.0,     -- RD: v1 字段覆盖度；无结构化证据 = 0
    recorded_at TEXT DEFAULT ''
);

CREATE TABLE compositions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    description TEXT DEFAULT '',
    proc_names TEXT DEFAULT '',    -- 逗号分隔
    created_at TEXT DEFAULT ''
);

CREATE TABLE patches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline TEXT NOT NULL,
    proc_name TEXT NOT NULL,
    impl_name TEXT NOT NULL,
    field TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT DEFAULT '',
    UNIQUE(pipeline, proc_name, impl_name, field)
);
```

---

## 5. 同构发现

### 5.1 匹配规则

两个 proc 的 tag 集合**完全相等** → 同构。

### 5.2 提示时机

- `parse`：解析后展示与已注册 proc 的同构匹配
- `run`：执行前展示同构匹配
- `discover`：全局展示所有同构组（≥2 个成员且来自不同 pipeline）

### 5.3 提示格式

```
Isomorphism hints:
  search ≅ find [translate] — 网页检索
```

含义：当前 pipeline 的 `search` proc 与 `translate` pipeline 的 `find` proc 同构（tags 相同）。

---

## 6. 热补丁机制

### 6.1 生命周期

```
patch 命令 → SQLite patches 表（UPSERT）
                ↓
run 命令 → apply_patches(pl)
           1. db::load_patches(pipeline_name)
           2. pl.clone()
           3. 遍历 patch，按 (proc, impl) 匹配
           4. 覆盖字段值
           5. 执行修改后的 clone
                ↓
patch clear → DELETE FROM patches WHERE pipeline = ?
```

### 6.2 stderr 日志

应用补丁时输出：

```
[patch] search.web: enabled=false
[patch] summarize.s1: retry=5
```

---

## 7. DSL_RESULT 协议

### 7.1 脚本输出格式

任何通过 `run()` / `sh()` 执行的脚本，在 stdout 末尾加块即可：

```
##DSL_RESULT
key1=value1
key2=value2
##DSL_END
```

### 7.2 下游引用

```
@proc_name.field_name
```

引擎解析时提取对应字段值。找不到则输出 `<no field:proc.field>`。

### 7.3 实现细节

内部编码：`§§FIELDS§§k1=v1§§k2=v2§§RAW§§原始stdout`

字段提取：遍历 `§§` 分隔段，匹配 `key=value`，遇到 `RAW§§` 停止。

---

## 8. AI 调用者操作手册

### 8.1 新建 pipeline

1. 分析用户需求，确定 proc 链
2. 为每个 proc 设计备选路径（至少 1 条）
3. 为叶子 impl 声明 tags
4. 写 check 谓词
5. 保存为 `.pipeline` 文件
6. 先 `ductile check` 验证语法
7. `ductile run` 执行

### 8.2 调优 pipeline

1. 多次 `ductile run`，观察哪些路径失败
2. 对不稳定路径 `ductile patch <pipeline> <proc> <impl> enabled false`
3. 或 `ductile patch <pipeline> <proc> <impl> cost.latency 9999` 降低排名
4. `ductile run` 验证效果
5. 满意后 `ductile version save` 存快照

### 8.3 复用已有节点

1. `ductile import ./pipelines` 导入全部
2. `ductile search "关键词"` 搜索可用 proc
3. `ductile discover` 查看同构组
4. `ductile compose` 从零件库组装

### 8.4 错误排查

| 现象 | 排查方向 |
|------|---------|
| `Parse error` | 检查语法：括号匹配、逗号、`->` |
| `Type check errors: CostNegative` | cost 字段有负值 |
| `Type check errors: EmptyPlan` | proc 没有 plan 也没有 deliver |
| `All paths failed for proc: X` | 所有 impl 都失败，检查 bridge 脚本路径 |
| 路径总是不被选中 | 可能被 penalty 惩罚，检查 `ductile patch list` |
| 结构化字段为空 | 检查脚本是否正确输出 `##DSL_RESULT` 块 |

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
        .when(mode == "deep")          // 内联 when（impl 级）
        .disabled
        .stub,
      fallback_name -> body_function(args)
        .tags(#tag3, #tag4)
    )
    .when(@upstream_judge.score < 80)  // 块级 when（下推到全部 impls，内联优先）
    .foreach(source=@upstream_proc, var=item_name)
    .deliver(@upstream_proc)
```

> **v0.11 退役语法**（仍可解析但警告+忽略，不入 AST）：`.cost(latency=..., ...)`、
> `.ensure(result => ..., "...")`、`.check(result => ..., "...")`。
> cost/权重 → `.eval` 策略文件 + `--policy` 挂载（§2/§3）；质量门槛 → 独立 judge proc +
> 块级 `.when` 路由。**不要在新管线中使用。**

### 1.3 语法约定

- Pipeline 必须以 `Pipeline("name")` 或 `Pipeline("name", "desc")` 开头
- 每层缩进 2 空格（仅人类可读性，解析器不依赖缩进）
- `.plan(...)` 内多条路径用逗号分隔
- 路径格式：`name -> body`（`->` 前是 impl 名，后面是函数调用文本）
- `.tags()` 只在 impl（叶子）上声明
- `.deliver()` 的 proc 不执行，仅标记终端输出
- 标签用 `#` 前缀，标签集是 BTreeSet（有序、去重）
- 依赖关系自动推导：body 中 `@proc_name` 引用即声明依赖
- **`.when(cond)` 两种写法（v0.11.1）**：
  - 内联（impl 级）：`name -> body.when(cond)` 只作用于该 impl
  - 块级（proc 级）：`.when(cond)` 独立成行，下推到该 proc 全部未持有内联 when 的 impls；条件里的 `@ref` 并入 refs，egraph 据此建裁判→消费者边（保证裁判先执行）
  - 块级语法错误（空条件/括号不平衡）= 解析期硬错误，不再静默丢弃

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
- 输出：节点数、边数、**e-class 数、融合规则命中、CSE 别名**、并行组、关键路径、静态提取计划

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

### 2.12 promote / grow / scaffold（v0.9.x 自组织线）

```
ductile promote [days] [top] [--dry]   # 跨会话 MDL 晋升门
ductile grow [days] [top_import]       # 命令全文构式生长
ductile scaffold [query]               # 查询构式库
```

- **promote**：从 state.db 收割命令 → 两部码检验（晋升成本 8B/char+32b 选择码 vs 次数×节省）→ 自动入 `_harvested` pipeline。**计数单位是跨会话数 sessions（≥2 才是复用证据）**——会话内重复是 agent 调试迭代（探索），不是知识。账本表 `promotions`（UNIQUE(tag,cmd)，可审计可回滚）。`--dry` 只评不写。
- **grow**：行级 token 在 60d 命令全文语料上贪心 pair-merge（增益门>0，同 promote 账法），长出的多行构式入 `scaffolds`（source='grown'）。物理：脚手架住在行间，首行归一化会毁掉它。
- **scaffold**：按关键词查 scaffolds（LIKE，按 save_b 降序）。
- 表：`scaffolds(text, save_b, use_count, lines, source, imported_at)`；`use_count` 语义 = **迭代频率**（对召回排序有信息量），不是知识证据；知识证据 = promotions.count（跨会话数）。

### 2.13 version

```bash
ductile version save <file.pipeline> "change description"
ductile version log <file.pipeline>
ductile version diff <file.pipeline> <v1> <v2>
```

- 存储在 `~/.local/share/ductile/versions/<pipeline_name>/`

---

## 3. 执行引擎行为

### 3.0 e-graph 执行模式（v0.10）

启用方式（二选一）：

```
.proc("x").pick(egraph)    # 单 pipeline 内任一 proc 声明即全局生效
环境变量 DUCTILE_EGRAPH=1  # 全局开关
```

管线内部：

1. 每个 proc 一个初始 e-class；每个 impl 投影为 e-node `(op, children)`，op = body 检测到的函数名，children = `@ref` 的 canonical class id
2. **equality saturation**（union-only，canonical class 数单调下降 → 必然终止，上限 64 轮）：
   - **R1 同构合并**：两 class 的 canonical 节点集一致 → union
   - **R2 merge 扁平化**：merge 节点的传递扁平子集相等 → union（涵盖交换律 `merge(@a,@b)≡merge(@b,@a)`、嵌套折叠 `merge(merge(a,b),c)≡merge(a,b,c)`、退化 `merge(@x)≡x`）
   - **R3 write/read 对消**：`read(from=P)` ≡ `write(to=P, content=@src)` 的 `src`（即 write→read 融合为 pass-through，路径精确匹配，`{topic}` 等占位符原样比较）
   - **when-载体守卫（v0.11.1）**：挂 `.when()` 的 impl 投影为 when-载体节点，R1/R2/R3 一律不熔合含载体的 class——熔合会抹掉裁判依赖序（judge→consumer 边消失，deliver 抢跑、判决落空）。纯等价 proc 的 CSE 能力不受影响
3. **提取器**：class 级 Kahn 拓扑（确定性）→ 逐 class 选最小静态 `cost_total` 的 enabled impl；对消 class 中 read 节点降级为缓存路径（class 内存在非 read 节点时不参与首选竞争）
4. **CSE**：同 class 只执行代表 proc，其余成员共享结果（执行日志 `[egraph]` 行可见 classes/融合命中/别名表）

两层分工：e-graph 决定"谁跑"（class 代表/序/别名），exec_proc 内部仍按历史惩罚/偏好排序决定"怎么跑"（impl 序 + retry）。默认关闭——不声明时行为与 v0.9 完全一致。

### 3.1 执行顺序（legacy 默认）

1. `apply_patches(pl)` — 从 SQLite 加载补丁，克隆并覆盖
2. `build_egraph(pl)` — 构建 e-graph（v0.10 起为真 e-class 结构，调度视图兼容旧接口）
3. `parallel_groups(eg)` — 拓扑分层
4. 逐层执行（层内串行）

### 3.2 路径选择算法

对每个 proc：

```
eligible = plan.filter(impl => when_condition_passes)
ranked = eligible.sort_by(score)

score(impl) = base_cost × (1 + penalty) / pref
```

- `base_cost = 0.001×latency + 10×risk + 0.0001×tokens + 1×money`
- `penalty` 见下表
- `pref` = 乘性学习权重（成功 ×1.1 / 失败 ÷1.5，clamp [0.05, 20]）

### 3.3 滑动窗口惩罚

每个 impl 保留最近 20 次执行记录。

| 条件 | penalty 值 | 效果 |
|------|-----------|------|
| 无记录 | 0.0 | 原始排序 |
| 窗口 < 3 | 0.0 | 冷启动探索 |
| 失败率 ≤ 10% | 0.0 | 无惩罚 |
| 失败率 > 10% | `e^(7×rate) - 1` | 指数惩罚 |
| 连续失败 ≥ 3 | `∞` | 永久 BLOCKED |

**惩罚域**：penalty 只管瞬时故障（可 retry 恢复的失败）。

### 3.7 评价策略（.eval，v0.11「裁判分离」）

评价与流程分离：`.pipeline` 只描述流程；权重与 cost 来源写在 `.eval` 策略文件，运行时挂载：

```
ductile run x.pipeline "topic" --policy strict.eval
```

`.eval` 行式格式（`#`/`//` 注释）：

```
weights latency=0.001 risk=10.0 tokens=0.0001 money=1.0
fail_closed = true
render.sdxl cost latency=measure("pipelines/bench_sdxl.sh {topic}") risk=0.05
render.flux  cost latency=5000.0
```

- cost 值两形态：直接数值，或 `measure("命令")` 链接测试脚本——引擎执行脚本取 stdout 第一个浮点数（**latency 单位：毫秒**），按 `(proc, impl, field)` 缓存进 `cost_cache` 表，TTL 24h
- 未挂载 `--policy` 时用引擎默认权重与 `.plan()` 顺序（不读 `.cost()`——v0.11 起 `.cost()/.check()/.ensure()` 均退役，解析期警告+忽略）
- 幽灵 impl 防御：`.plan()` 内非 `name ->` 条目（如旧 SPEC 幻觉语法 `weights(rd=N)`）现在硬错误，不再静默生成 path_N 假成功
- 未知函数 fail-closed：`<noop>` 假成功已删除，不可识别的函数体触发 Err 正常降级
- 质量门槛新形态：独立 judge proc 输出 `##DSL_RESULT` 结构化字段 + `.when(@judge.score < 80)` 路由（`.when` fail-closed）

### 3.6 运行测量（v0.11 接线）

每次执行自动测量并落库：

- `rate_tokens` = 输出字符数 / 4（token 代理）
- `latency_ms` = Instant 实测（v0.10 恒 0，v0.11 起真实值）——"cost 从测量来"的地基
- `est_loss` v1 字段覆盖度函数保留（`est_loss_field_coverage`），供 judge proc 评测用；排序公式不再使用 RD 附加费（`weights.rd` 已随 v0.11 移除）

### 3.4 retry

`.retry(n=3)` 失败后等待 `2^(attempt+1)` 秒：2s → 4s → 8s。共尝试 n+1 次。

### 3.5 check 流程（已退役）

v0.11 起谓词层退役（裁判与生产分离）：`.check()` / `.ensure()` 解析期降级为警告+忽略，不影响执行。
质量门槛改用独立 judge proc + `.when` 路由，见 3.7。

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

-- v0.8.1+ 自组织线 (harvest/wrap) 与 v0.9.x (promote/grow/scaffold)
CREATE TABLE wrapped_cmds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tag TEXT NOT NULL, cmd TEXT NOT NULL,
    exit_code INTEGER, recorded_at TEXT DEFAULT '',
    UNIQUE(tag, cmd)
);

CREATE TABLE promotions (          -- 跨会话 MDL 晋升账本 (v0.9.0+)
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tag TEXT NOT NULL, cmd TEXT NOT NULL,
    count INTEGER,                 -- 跨会话数 (sessions): ≥2 = 复用证据
    gain_bits REAL,                -- 两部码净收益
    promoted_at TEXT DEFAULT '',
    UNIQUE(tag, cmd)
);

CREATE TABLE scaffolds (           -- 构式库 (V26 生长, v0.9.2+)
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    save_b INTEGER,                -- 总节省 bits
    use_count INTEGER,             -- 迭代频率 (非知识证据, 见 §2.12)
    lines INTEGER,
    source TEXT DEFAULT 'grown',   -- 'grown' = ductile grow; 'v26' = 首次导入
    imported_at TEXT DEFAULT '',
    UNIQUE(text)
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

## 6a. 资源管理算子（v0.7）

### 6a.1 进程管理

```
spawn(name="handle", cmd="long-running-cmd")   // 后台启动，返回 pid
procs(name="handle")                           // 列句柄: name pid age_s alive cmd
kill(name="handle")                            // SIGKILL 整个进程组（防孤儿）；也接受 kill(name="1234") 裸 pid
wait(name="handle", timeout=60)                // 阻塞等句柄退出；超时报错
```

- 句柄注册进全局进程表（进程内 OnceLock），pipeline 结束后进程表不持久化
- `kill` 用 `kill -9 -PID`（负 pid = 进程组），确保 `bash -c "foo &"` 的孤儿子进程一并回收
- liveness 检查走 `/proc/<pid>`，无平台依赖

### 6a.2 文件系统

```
exists("path")          // → "true" / "false"
stat("path")            // → "kind size_bytes mtime_unix"（kind = file|dir|other）
ls("dir")               // → 每行一个条目名，排序后
cp(from="src", to="dst") // 文件直拷；目录树 shell 出 cp -r
mkdir("path")           // create_dir_all，父目录自动建
rm("path")              // 递归删除；拒绝 / 和 $HOME 根（硬保护，不可关闭）
```

### 6a.3 磁盘与 run() 增强

```
disk("/tmp")            // → "avail_gb total_gb"（df -BG 解析）

run("cmd", timeout=120)             // 超时杀进程组后报错；默认 300s；timeout=0 不限
run("cmd", env="K1=V1" env="K2=V2") // 重复 env 注入；PATH/HOME/USER 防覆写
```

### 6a.4 安全边界

- `rm` 的根保护是编译期常量行为，无开关
- `kill` 只能杀本 pipeline spawn 的句柄，或显式给出的 pid
- `env` 注入不碰身份变量（PATH/HOME/USER）
- 全部路径走 `expand_tilde`，支持 `~/...`

### 6a.5 示例

`~/notes/obsidian/default/个人系统/流程pipeline/resource-demo.pipeline`（仓库外 dogfooding 副本）：disk-check → spawn/kill → mkdir/cp/stat/ls/rm → timeout 降级，四 proc 全通过。

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
| `script 'X' not attached` | 先 `ductile script attach <file>` 注册 |
| `param 'k' not in contract` | 调用传了契约未声明的参数，`ductile script show X` 查契约 |
| `missing required param` | 必填参数缺失或为空 |

---

## 9. 脚本契约线（v0.12 — 脚本即 API）

运行层脚本外挂机制：**脚本本体不写入 DSL，DSL 只留 `script(name, k=v...)` 链接**；
元数据经结构化契约头自描述，LLM 调用时只读契约卡、不读脚本体。

### 9.1 契约头（脚本写作结构）

脚本文件头部用注释行声明（`#` 开头，sh/python/powershell 通用）：

```
# ductile: v1
# name: word_stats                 ← 注册名（DSL 引用它）
# desc: 一句话描述
# lang: python                     ← bash | python | powershell（可扩展）
# params: text(str, required), n(int, default=10)
# output: words(int), lines(int)
# pure: true                       ← 有无副作用（纯函数=true）
# idempotent: true                 ← 重复执行结果是否不变
# concurrency: safe                ← safe | exclusive（能否并发）
# effects: none                    ← none | fs | net | system
# timeout: 10                      ← 秒；# retries: N 可选
```

**语义标注的编排意义**（喂给引擎做自动并行/CSE 决策）：
`pure=true && idempotent && concurrency=safe` → `cse_safe=true`（可安全 CSE/并行）；
副作用脚本（`pure=false` 或 `concurrency=exclusive`）→ 不可熔合、不可并行，
防止两个同构副作用节点被 CSE 熔成一个导致双执行变单执行。

### 9.2 CLI

```
ductile script attach <file>            注册（解析契约头，upsert）
ductile script list                     列出全部（NAME/LANG/PURE/IDEMPOTENT/CONCUR/EFFECTS）
ductile script show <name>              契约卡（LLM 读这个，不读脚本）
ductile script call <name> "k=v, k=v"   单发调用（调试）
ductile script detach <name>            注销
```

### 9.3 DSL 调用与执行语义

```
.proc("analyze")
  .plan(
    py -> script(word_stats, text="{topic}")
  )
```

- 契约卡从 scripts 表加载；未注册的脚本名 fail-closed 硬错（错误信息列出全部已注册脚本）
- `k=v` 值支持 `{topic}`、`@proc.field` 上游引用、契约 `default=X` 兜底
- **必填参数缺失/为空 → 硬错；契约未声明的幻觉参数 → 硬错**（契约即接口）
- 传参经环境变量 `DUCTILE_ARG_<NAME>`、`DUCTILE_TOPIC`；脚本侧 `getenv` 取参
- 输出复用 `##DSL_RESULT` 协议（见 §7）；无协议块时整体 stdout 作为结果（>5000 字符截断）
- timeout/retries 走契约头

### 9.4 脚本写作规范（LLM 生成脚本必读）

1. **bash 必须 `set -euo pipefail`**——否则中间步骤失败被吞、最后的 echo 返回 0，
   引擎收到 exit 0 → 静默半成功（实测踩过的坑）
2. 输出机器可读结果一律走 `##DSL_RESULT` 块，人类可读信息走 stderr
3. 契约头如实标注 pure/concurrency——标注撒谎会让编排优化破坏正确性
4. 幂等脚本注明 `idempotent: true`，引擎可安全重试

示例：`examples/scripts/word_stats.py`（纯函数）、`examples/scripts/make_report.sh`（fs 副作用）；
验收链路：`pipelines/script_demo.pipeline`。

---

## 10. 模块结构与测试架构（v0.12.1 — 全系统解耦）

### 10.1 模块地图

```
src/
├── parser.rs     # DSL 解析（.pipeline 主体 + .eval 策略文件）
├── ast.rs        # 类型（Impl/Proc/Pipeline/Policy/CostSpec/CostValue）
├── typecheck.rs  # 类型检查
├── executor.rs   # 编排主干（exec_pipeline/exec_proc/foreach/retry/patches）— 697 行
├── steps.rs      # step_registry 注册表（清单即表）+ 21 个内置执行器
│                 #   进程：spawn/procs/kill/wait（PROC_TABLE + process_group(0) 自立进程组）
│                 #   文件：exists/stat/ls/rm/cp/mkdir/disk（rm 拒绝 / 与 $HOME）
│                 #   读写：read/write/run/sh + search/llm/merge + script(name,...)
├── textargs.rs   # 纯文本解析原语：detect_func/resolve_vars(@proc.field)/extract_*
├── dslresult.rs  # ##DSL_RESULT 协议：parse_dsl_result_block/encode/extract_field/est_loss
├── ranking.rs    # 偏好学习（ImplPrefs 乘性权重 ×1.1/÷1.5 clamp[0.05,20]）+ 排序 + 失败惩罚
├── eval.rs       # 裁判分离运行时：Evaluator/CostSource(measure 实测)/CostCacheStore
├── egraph.rs     # e-graph 等价类 + CSE + when-载体熔合守卫
├── db.rs         # SQLite 层：22 个 *_conn(conn) 注入内核 + 全局薄壳；SCHEMA_DDL 单一事实源
├── script.rs     # v0.12 脚本契约：契约头解析/lang 解释器/cse_safe 判定
└── cli.rs        # 命令分发 + 纯参数解析（split_run_args/parse_promote_args）
```

兼容性：executor 对拆出符号保留 re-export（`executor::detect_func` 等旧路径不变）。

### 10.2 测试架构（271 个，三层）

| 层 | 对象 | 手法 |
|----|------|------|
| 纯函数直测 | 解析原语/JSON 解析器/协议编解码/参数解析 | 无 I/O 断言输入输出 |
| 内存库回路 | db 层 22 函数 | `Connection::open_in_memory` + SCHEMA_DDL，零真实库污染 |
| 真实子进程集成 | run/spawn/kill/wait/fs 算子 | 临时目录 + setsid 进程组隔离 |

测试演进：162（v0.12.0）→ 262（v0.12.1 解耦）→ 271（v0.13.1 现值，峰值 277 含已删除的 serve 测试）。解耦过程暴露并修复 6 个潜伏 bug：

1. `exec_spawn` 未自立进程组 → `kill -9 -pid` 组杀目标组不存在、静默无效
2. `exec_wait`/`exec_procs` 以 `/proc` 存在性判活 → 僵尸进程死等超时
3. `dslresult::extract_field` RAW 终止符死分支（`starts_with("RAW§§")` 在 `split("§§")` 后永假）→ 字段提取越界进原始 stdout
4. executor 残留 v0.11 迁移死代码岛 ~135 行（apply_policy/resolve_cost_value/first_f64）
5. `parse_promote_args` 按类型分支派发 → 第二位置参数从未生效（任何 u32 都可 parse 成 usize）
6. `harvest::jparse_str` UTF-16 代理对 off-by-one（`+6+1` 重复计数）→ 串尾代理对返回 None、后随字符被吞

### 10.3 db 层注入模式

数据函数双形态：

```rust
// 内核：测试注入内存连接
pub fn record_run_rd_conn(conn: &Connection, proc_name: &str, ...) { ... }

// 薄壳：生产路径（全局库）
pub fn record_run_rd(proc_name: &str, ...) {
    let conn = open();
    record_run_rd_conn(&conn, proc_name, ...)
}
```

新增数据函数一律遵循此模式；表结构改动改 `SCHEMA_DDL` 常量（init_db 与测试共用）。

### 10.4 脚本写作的测试纪律

- 测试数据含引号/反斜杠/代理对时，用**字符字面量数组**构造（`vec!['"', '\\', 'n', ...]`），
  不用 raw string（`"#` 定界符与 `\"` 序列撞车）与双层转义
- 覆盖率工具（tarpaulin）与 pyo3 不兼容（debug 链接缺 libpython）——以测试面清单为准

---

## 11. API 层与 LangChain 集成（v0.13）

### 11.1 架构：core + 薄壳双形态

所有对外 API 函数是双形态：`*_core` 纯 Rust（返回 `String` / `Result<String,String>`），
pyo3 `#[pyfunction]` 薄壳只做包装。**bin/test 内部代码必须调 `*_core`**——`#[pyfunction]`
符号会把 pyo3 运行时拉进 bin/test 链接图，而 `extension-module` feature 不链
libpython（实测：rust-lld `undefined symbol: _Py_Dealloc`）。core 路径下 pyo3
代码被 `--gc-sections` 丢弃，bin 与 wheel 各自干净。

```
run_json / script_call_json / pipeline_json → Result<String,String>（core）
scripts_json / procs_json / runs_json / db_stats_json → String（core，永不失败）
```

### 11.2 语义约定：失败是数据

- **创作错误**（文件不存在/解析/类型检查/策略非法）→ `Err` / Python 异常
- **执行失败**（所有路径失败/degraded）→ `Ok({"ok":false,"error":...})`
  —— agent 与前端按 JSON 路由，不靠异常捕获
- `script_call_json` 未注册脚本名 → `{"ok":false}` + 已注册清单（不抛）
- 输出字段解码：`§§FIELDS§§` 内部编码 → 干净 `fields` 对象（RAW 边界防泄漏）

### 11.3 LangChain 适配（python/ductile 混合包）

```
pip install ductile && pip install langchain-core
ductile.langchain_tools() → [BaseTool]
```

工具集 = 6 内省/执行工具 + 每个已注册脚本契约一个类型化工具（docstring/参数
schema 从契约卡生成）。已知坑（实测）：

1. Pydantic v2 保留参数名 `args` 会被别名化（`v__args`）→ invoke TypeError——工具参数命名避开 `args`
2. `@tool` 装饰时函数必须已有 docstring（先赋 `__doc__` 再装饰）
3. `**kwargs` 无法被 Pydantic 内省成 schema → 用 `inspect.Signature` 注入契约参数

---

## 12. 全自动错误处理（v0.14 — errflow.rs 统一模块）

错误处理零 DSL 面、零配置面（用户裁定）：分类→策略→响应全部引擎内建。

### 12.1 错误分类（classify，v0.14c 十二类）

判定链先具体后兜底：
cancelled→contract→ratelimit→auth→timeout→memory→dependency→permission→network→resource→data→crash

| code | 锚点示例 | 划界理由 |
|------|---------|---------|
| `cancelled` | `KeyboardInterrupt`、`SIGINT` | 用户意图，禁自动重试 |
| `timeout` | `run timed out after Ns`、`ReadTimeout` | 慢 |
| `ratelimit` | `429`、`rate limit`、`quota exceeded` | 限流窗口，退避后有效 |
| `auth` | `401`、`403`、`invalid api key`、`token expired` | 凭证坏，换供应商 |
| `memory` | `CUDA out of memory`、`exit 137`、`MemoryError` | 显存/内存瞬时占用 |
| `dependency` | `ModuleNotFoundError`、`command not found`、`shared libraries` | 环境确定性坏 |
| `network` | `Connection refused`、`getaddrinfo failed`、`502/503/504` | 网络抖动（与本地缺失分离） |
| `resource` | `file not found`、`spawn failed`、`ENOSPC` | 本地确定性缺失 |
| `permission` | `Permission denied`、`EACCES`、`EPERM` | 权限 |
| `data` | `TypeError`、`ValueError`、`JSONDecodeError` | 数据错，换路径 |
| `contract` | `script not attached`、`param not in contract` | 创作错误 |
| `crash` | 其余一切（segfault、panic） | 兜底 |

### 12.2 失败节点策略（strategy）

| code | action | 理由 |
|------|--------|------|
| cancelled | `Escalate` | 用户意图，禁止任何自动重试 |
| timeout | `Retry{2, linear}` | 瞬时资源紧张常自愈 |
| ratelimit | `Retry{2, linear}` | 限流窗口退避 |
| auth | `Escalate` | 凭证坏重试无意义（换供应商走 Switch 响应） |
| memory | `Retry{1, linear}` | 等显存释放一轮 |
| network | `Retry{2, linear}` | 抖动自愈概率高 |
| resource | `Retry{1, linear}` | 一轮竞态缓冲 |
| data | `Reroute` | 同输入必再错，立即换 impl（impl.retry 剩余预算直接跳过） |
| dependency | `Escalate` | 环境确定性坏 |
| permission | `Escalate` | 重试无意义 |
| contract | `Escalate` | 创作错误 fail-fast |
| crash | `Escalate` | 未知根因不瞎猜 |

### 12.3 正常节点响应（respond，按错误进程的判断进行）

| code | response | 行为 |
|------|----------|------|
| cancelled | `Exit` | 用户已表态，整流立即停 |
| timeout/ratelimit/memory/network/resource | `Wait` | 上游吃满 Retry 预算（分层顺序天然提供）后按传播处理 |
| auth/data | `Switch` | 只封锁引用死源的 impl，未引用备选接管（auth=换供应商，data=换路径） |
| dependency/permission/crash | `Ignore` | 无关 proc 照跑；引用者落传播 Left |
| contract | `Exit` | 立即退出整个流程（partial 保留） |

### 12.4 失败是值（Either 内部语义）

- proc 失败不中止管线：`results[p] = §§FIELDS§§err=1§§err_code=…§§ 编码`（Left）
- 下游引用死源且无可切换方法 → 传播 Left（`err_msg=propagated from X`，err_code 继承）
- 任何 Left → `run` exit 1；`run_json` 输出 `{"ok":false,"err_code":…,"partial":{…}}`
- partial 中 Left 解码为干净对象，Right 原样——debug/agent 不丢现场

### 12.5 关键过程/关键节点判定（v0.14b）

引擎自动判定节点关键性并调整行为（零 DSL 面）：

- **critical_set**：`.deliver(@x)` 引用闭包 BFS 回溯——deliver→x→x 的 refs 逐层上溯，
  主产出链上的 proc = 关键节点；不在链上 = 旁路（日志/通知/监控类）。
  无 deliver proc 的管线 = 全部关键（v0.9 兼容）。
- **strategy_for(code, critical)**：关键节点 Retry 预算 ×2（主链值得更努力），旁路基础预算
- **fatal_left 终局裁决**：只有关键 proc 上的 Left 致命（`critical proc 'x' failed`）；
  旁路失败容忍——主产出不受牵连，流水线仍 Success（stderr 明示
  `bypass failures tolerated`，旁路 Left 仍在 results/partial 可查）
- parser 变更：`.deliver(@x)` 参数旧版只置 is_deliver 即丢弃——现解析进
  `Proc.deliver_refs`（v0.4 起的潜伏遗漏，关键性判定的地基）

# MCSM/FOPT — 模型建构状态机的 ductile 认知接口

> v0.18.11。用户建构理论（MCSM v2.0）的工程落地：任何被建构的 ductile 对象
> ——脚本、节点、管线、拓扑、层——都可以用四维认知坐标 F-O-P-T 标注它
> 当前处在认知操作循环的哪一步。**数字不是价值高低，是当前操作**：
> 1=建表（被动积累）2=冲突（发现剩余）3=抽象（构造解释）4=实践（实证干预）。

## 为什么进引擎而不是留在文档

制度>自觉：脚本契约头 `# mcsm:` 是 fail-closed 校验的（垃圾坐标拒绝注册），
`ductile script show` 直接展示——LLM 调用者读卡即知对象认知状态，不用翻文档。
管线/拓扑/层的标注走注释约定（见下），由人读、由 hyper 相似度索引。

## 四维语义（F-O-P-T）

| 维 | 名 | 存在论问题 | 学科 | ductile 对应物 |
|---|---|---|---|---|
| F | 场域 Field | 模型在什么存在论边界内有效？ | 存在论的空间性 | 依赖图边界：本管线可引用的 proc/agent/script 全集 |
| O | 本体论 Ontology | 承认什么算存在？ | 存在论的内容 | 对象类型：proc / impl / script / pipeline / hyper-node / 层 |
| O 显现 | 现象 Phenomenon | 存在如何被给予？ | 存在论的显现 | DSL_RESULT 协议、契约卡、runs 表——对象如何向你显现 |
| T | 目的论 Teleology | 运动指向什么终极目的？ | 存在论的方向性 | deliver 节点、.when 门禁、盲评/calibrate 的目的锚点 |

## 对象类型 × FOPT 快查

| ductile 对象 | F（场域） | O（本体） | P（现象） | T（目的） |
|---|---|---|---|---|
| script | 依赖的命令行工具与文件系统环境 | 契约卡声明的 params/output 类型 | `##DSL_RESULT` 块；`script show` 卡 | 契约 desc 承诺的单次变换 |
| proc（节点） | 上游 proc 集合（@ref 可达） | impl 多实现 + tags 同构类 | llm 的 kvs / run 的 exit+stdout | `.when` 守卫的下游契约 |
| pipeline | cwd + config.toml 场域 | proc 集合 + 依赖边 | `ductile run` 日志 + degraded 标志 | deliver 节点列表 |
| hyper-node（拓扑） | 所属 .hyper 图 | node 的 tags + 结构签名 | `hyper similar` 匹配提示 | 结构复用（写新图前先 `hyper similar`） |
| 层（L0-L4） | 上层可依赖的下边界 | 模块 + 层间依赖铁律 | layers_probe 探针（selftest 第10道） | 高内聚低耦合的归层理由 |

## 复合坐标与迁移

格式：`F(f)-O(o)-P(p)-T(t)`，各维独立 1-4。例：

```
F(2)-O(1)-P(3)-T(2)  ← mingli r10 末期的读层节点：
  场域在冲突（位次轮换的边界裂缝未解），
  本体仍建表（盘面九字段枚举中），
  现象已抽象（tier_journal 经验账本显现象），
  目的在冲突（真盘胜率26%——盲评目的自身被证伪中）。
```

迁移规则（各维独立循环 1→2→3→4→1）：建表中出现无法归类的异常→2；
找到能容纳旧表+异常的新结构→3；新结构被实际操作→4；实践产生新经验→1。

## 落点

- **脚本**：契约头 `# mcsm: F(2)-O(1)-P(3)-T(2)`（可选键，fail-closed 校验，
  大小写不敏感，规范化大写存储）。`script show` 展示。
- **管线**：文件头注释 `// mcsm: F(1)-O(2)-P(1)-T(3)`（约定，parse 不拦）。
- **hyper 拓扑**：node 定义行尾注释（约定）。
- **commit / devcycle**：topic 字符串内嵌坐标（自由文本惯例）。

## 与认知分层论的同构

MCSM 操作循环（1建表→2冲突→3抽象→4实践）与 ductile 认知分层
（L0 机器码→L3 DSL→NL）共享同一个内核：**每一层都有自己的 1-2-3-4**。
层是空间切片，FOPT 是时间切片；F-O-P-T 各维在自己的层内独立循环，
高层结构（L4 hyper）是低层循环 3-抽象期的沉淀物。
`docs/LAYERS.md` 答"代码放哪"（静态），本文档答"对象在认知循环哪一步"（动态）。

## 实例打标（真实对象，非示例）

| 对象 | 坐标 | 依据 |
|---|---|---|
| `scripts/paipan_all.py`（mingli） | F(4)-O(4)-P(4)-T(4) | 与 v1 存量 100% 一致——四维全在实践期 |
| `m2_ring.pipeline`（mingli r10） | F(2)-O(1)-P(3)-T(2) | 位次混淆未破（F 冲突）、九字段建表中、tally 已抽象、26% 胜率证伪目的 |
| 阶梯（v0.18.10 双向自适应） | F(3)-O(3)-P(4)-T(4) | 双向机制已抽象+实践（tier_journal 落账、feedback 注入实跑验证） |
| ductile 引擎自身 | F(3)-O(3)-P(3)-T(4) | 七层栈+FOPT 接口已抽象，selftest 428 绿实践验证中 |

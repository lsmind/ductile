# v0.18 消融实验（A1-A5）

五项引擎修复的 old/new 双臂消融。判据全确定性（文件内容/graph 边数/熔合计数/放行标志），无 LLM 参与。

## 设计

每项修复的 **old 臂 = 该修复提交的父提交二进制**，new 臂 = 112ccf3（v0.18.14）。
单一变量隔离：old 与 new 之间只差目标修复所在提交链。

| 消融 | 修复 | old 臂 | 探针 | old 表现 | new 表现 |
|---|---|---|---|---|---|
| A1 | v0.18.8 foreach source 依赖边 | 6329e1d | ab1: 消费名(acon)字典序先于源(zsrc) | degraded, 无输出文件 | 两项都迭代（beta） |
| A2 | v0.18.9 foreach var 运行时化 | 1b292e4 | ab2: item 含双引号 | 引号项丢失（只剩 has） | 完整存活（has"quote） |
| A3 | v0.18.13 when && 合取 | babff00 | ab3: 全 != 复合条件 | 静默永真放行（A3-RAN） | 正确拦截（LEFT+degraded） |
| A4 | v0.18.14 跨行 llm() continuation | 6c276dd | ab4: 跨行 prompt 引 @src | 依赖边消失（同平行组） | src→ask 分层（Edges:1） |
| A5 | v0.18.13 mcsm/cse_safe 禁熔合 | babff00 | ab5: 副作用脚本双 impl 同签名 | isomorphic_union 熔合, CSE aliases second→first | 无熔合（E-classes 3） |

结果 5/5 DIFF-CONFIRMED（result.json）。

## 复现

```bash
mkdir -p /tmp/abl/bin
cp ab*.pipeline abl_echo5.sh runner.py /tmp/abl/
# new 臂
cp <repo>/target/release/ductile /tmp/abl/bin/ductile-new
# old 臂: worktree checkout 对应提交后 cargo build --release
cp <old-worktree>/target/release/ductile /tmp/abl/bin/ductile-<sha>
cd /tmp/abl && python3 runner.py
```

runner.py 全程用 DUCTILE_DATA 隔离目录，不碰真库（~/.local/share/ductile/ductile.db）。

## 副产品：挖出 v0.18.14 残留 off-by-one

ab4 原始形态（多行 .proc 收尾行恰为文件末行）触发 v0.18.14 吞并循环的边界误报
（`idx+1+consumed_extra+1 >= lines.len()` 多算一位 → 合法输入报 unbalanced parens）。
已修（严格越界才拒）+ 两条回归测试钉死（parse_multiline_proc_ending_at_last_line /
parse_multiline_proc_genuinely_unbalanced_still_fails）。

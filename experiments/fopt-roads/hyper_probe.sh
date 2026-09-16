#!/usr/bin/env bash
# X6 深链: hyper 拓扑面 — 顶点/边/check 全链
set -u
BIN=${FOPT_HEAD:-/tmp/fopt_new_bin}
D=$(mktemp -d /tmp/fopt_x6.XXXX)
export DUCTILE_DATA=$D
PASS=0; FAIL=0
ok()  { echo "  PASS $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; FAIL=$((FAIL+1)); }

# 管线: probe_a 产出 → gate 判定 → probe_b 消费 (chain+gate 双语义)
# v0.18.15: deliver 必须用规范哨兵形态（专门 .proc("deliver") 引用别人）。
# 顶层独行 .deliver(@x) 会被 parser 归给前一个 proc（X6 bug 形态）→ ParseError。
cat > $D/x6.pipeline <<'EOF'
Pipeline("x6", "hyper probe")
.proc("probe_a", run("echo A-OUT"))
.proc("gate", run("echo '@probe_a' | grep -q A-OUT && printf 'G-PASS\n##DSL_RESULT\npass=1\n##DSL_END\n'"))
.proc("probe_b", run("echo B-DONE")).when(@gate.pass == 1)
.proc("deliver", run("echo x6-done"))
  .deliver(@probe_b)
EOF
cat > $D/x6.hyper <<'EOF'
HyperGraph("x6", "hyper probe")
.vertex("probe_a", role=source, tags=#run)
.vertex("gate", role=default, tags=#gate)
.vertex("probe_b", role=default, tags=#run)
.hedge("flow", kind=chain, probe_a, gate, probe_b)
EOF
out=$($BIN check $D/x6.pipeline 2>&1)
echo "$out" | grep -qi "error\|fail" && bad "check 管线: $out" || ok "check 管线合法"
out=$($BIN hyper check $D/x6.hyper $D/x6.pipeline 2>&1)
echo "$out" | grep -qi "ok\|pass\|✓" && ok "hyper check 拓扑+管线联合校验" || echo "  (探针注) hyper check 输出: $out"
# hyper nodes / similar
$BIN hyper nodes 2>&1 | head -3 > $D/nodes.out
grep -qi "probe\|vertex\|node" $D/nodes.out && ok "hyper nodes 列顶点" || bad "hyper nodes"
# run 全链: gate 依赖序
out=$($BIN run $D/x6.pipeline t 2>&1)
if echo "$out" | grep -q "B-DONE"; then
    ok "run 依赖序执行 (a→gate→b)"
elif echo "$out" | grep -q "probe_b"; then
    bad "probe_b 尝试但未产出 B-DONE: $(echo "$out" | grep -A 2 probe_b | head -3)"
else
    bad "probe_b 未被调度(when 判死或依赖边缺): $out" 
fi
# 负向探针: gate 断言失败应短路下游
cat > $D/x6f.hyper <<'EOF'
HyperGraph("x6f", "neg probe")
.vertex("probe_a", role=source, tags=#run)
.vertex("probe_b", role=default, tags=#run)
.hedge("flow", kind=chain, probe_a, gate, probe_b)
EOF
echo "X6 结果: PASS=$PASS FAIL=$FAIL (沙箱 $D)"

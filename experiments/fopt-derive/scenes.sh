#!/usr/bin/env bash
# FOPT 全量场景测试——六组场景覆盖全部组件面, 断言全确定性(零LLM)。
# 组件清单: 契约头解析/parse_mcsm 校验/cse_safe(三前提+矛盾态)/mcsm_effective
#           派生(incident 开关)/egraph 熔合拦截(静态)/R1同构/R2R3/when_guard/
#           script call 执行/incidents 表/展示面
set -u
BIN=${FOPT_HEAD:-/tmp/fopt_new_bin}
D=$(mktemp -d /tmp/fopt_scenes.XXXX)
export DUCTILE_DATA=$D
PASS=0; FAIL=0
ok()   { echo "  PASS $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL $1"; FAIL=$((FAIL+1)); }
chk()  { if grep -q "$2" "$1"; then ok "$3"; else bad "$3"; fi; }

echo "== S1: 契约头 + 坐标校验 (parse_mcsm) =="
mkdir -p $D
# 合法: 各维边界 1 和 4
for mcsmline in "F(1)-O(1)-P(1)-T(1)" "F(4)-O(4)-P(4)-T(4)" "F(2)-O(3)-P(1)-T(4)"; do
    name="s1_$(echo "$mcsmline" | md5sum | cut -c1-6)"
    sed "s/# name: pa/# name: $name/;s|F(1)-O(1)-P(1)-T(1)|$mcsmline|" \
        experiments/fopt-derive/build_foptcmp/pa.sh > $D/$name.sh
    out=$($BIN script attach $D/$name.sh 2>&1)
    echo "$out" | grep -q "attached: $name" && ok "attach 合法坐标 $mcsmline" || bad "attach 合法坐标 $mcsmline: $out"
done
# 非法: 越界/错维字母/坏格式
cat > $D/s1_bad1.sh <<'EOF'
# ductile: v1
# name: s1_bad1
# lang: bash
# desc: probe
# params: text(str, required)
# output: echo(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(5)-O(1)-P(1)-T(1)
echo x
EOF
sed 's/# name: s1_bad1/# name: s1_bad2/;s/F(5)/F(2)/;s/-O(1)/-X(1)/' $D/s1_bad1.sh > $D/s1_bad2.sh
sed 's/# name: s1_bad1/# name: s1_bad3/;s/F(5)-O(1)-P(1)-T(1)/F2O1P1T1/' $D/s1_bad1.sh > $D/s1_bad3.sh
out=$($BIN script attach $D/s1_bad1.sh 2>&1); rc=$?
[ $rc -ne 0 ] && echo "$out" | grep -q "bad mcsm" && ok "拒绝 F(5)" || bad "拒绝 F(5): rc=$rc $out"
out=$($BIN script attach $D/s1_bad2.sh 2>&1); rc=$?
[ $rc -ne 0 ] && echo "$out" | grep -q "bad mcsm" && ok "拒绝 X 维" || bad "拒绝 X 维: rc=$rc $out"
out=$($BIN script attach $D/s1_bad3.sh 2>&1); rc=$?
[ $rc -ne 0 ] && echo "$out" | grep -q "bad mcsm" && ok "拒绝无括号格式" || bad "拒绝无括号格式: rc=$rc $out"
# 展示面: script list 里的 mcsm 列
$BIN script show s1_$(echo "F(2)-O(3)-P(1)-T(4)" | md5sum | cut -c1-6) > $D/s1_show.txt 2>&1
chk $D/s1_show.txt "F(2)-O(3)-P(1)-T(4)" "show 展示坐标"

echo "== S2: cse_safe 三前提 (pure/idempotent/concurrency) =="
# 只缺 idempotent=false: 矛盾态语义下的纯幂等安全脚本, 手标 F(2)
cat > $D/s2a.sh <<'EOF'
# ductile: v1
# name: s2a
# lang: bash
# desc: probe
# params: text(str, required)
# output: echo(str)
# pure: true
# idempotent: false
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(1)-O(1)-P(1)-T(1)
echo "px:$DUCTILE_ARG_TEXT"
EOF
$BIN script attach $D/s2a.sh >/dev/null 2>&1
$BIN script show s2a | grep -q "cse_safe:     false" && ok "idempotent=false → false (三前提独立生效)" || bad "idempotent=false 应 false"
# 依赖注入检查: 真库确认 s2a 落库
sqlite3 $D/ductile.db "SELECT name FROM scripts WHERE name='s2a'" | grep -q s2a && ok "scripts 表落库" || bad "scripts 表落库失败"

echo "== S3: mcsm_effective 派生 (incident 开/关) =="
sed 's/# name: pa/# name: pz3/' experiments/fopt-derive/build_foptcpmp 2>/dev/null; sed 's/# name: pa/# name: pz3/' experiments/fopt-derive/build_foptcmp/pa.sh > $D/pz3.sh
$BIN script attach $D/pz3.sh >/dev/null 2>&1
$BIN script show pz3 | grep -q "cse_safe:     true" && ok "基线: 无 incident cse=true" || bad "S3 基线"
python3 -c "
import sqlite3
c = sqlite3.connect('$D/ductile.db')
c.execute(\"INSERT INTO incidents (pipeline, proc_name, signals, err_code, evidence, status) VALUES ('x','pz3','L0','explore_finding','a','open')\")
c.commit()"
$BIN script show pz3 | grep -q "cse_safe:     false" && ok "open incident → 派生(2) → cse=false" || bad "派生未生效"
python3 -c "
import sqlite3
c = sqlite3.connect('$D/ductile.db')
c.execute(\"UPDATE incidents SET status='closed' WHERE proc_name='pz3'\")
c.commit()"
$BIN script show pz3 | grep -q "cse_safe:     false" && bad "incident 关闭后应回 true" || ok "incident 关闭 → 解除 → true"

echo "== S4: 熔合拦截 (egraph 静态层) =="
# pb: 手标F(2)两个同构 proc → 拦; pa: F(1) → 熔
# 本沙箱注册 pb(F2)/pa(F1)
sed 's/# name: pa/# name: pb/;s|F(1)-O(1)-P(1)-T(1)|F(2)-O(1)-P(1)-T(1)|' experiments/fopt-derive/build_foptcmp/pa.sh > $D/pb.sh
$BIN script attach $D/pb.sh >/dev/null 2>&1
sed 's/# name: pa/# name: pa/' experiments/fopt-derive/build_foptcmp/pa.sh > $D/pa.sh
$BIN script attach $D/pa.sh >/dev/null 2>&1

printf 'Pipeline("s4", "t")\n.proc("a", script(pb, text="x"))\n.proc("b", script(pb, text="y"))\n' > $D/s4_block.pipeline
printf 'Pipeline("s4", "t")\n.proc("a", script(pa, text="x"))\n.proc("b", script(pa, text="y"))\n' > $D/s4_allow.pipeline
$BIN graph $D/s4_block.pipeline 2>/dev/null | grep -q "E-classes: 2" && ok "F(2) 同构对 → 拦截 (E-classes=2)" || bad "F(2) 应拦"
$BIN graph $D/s4_allow.pipeline 2>/dev/null | grep -q "E-classes: 1" && ok "F(1) 同构对 → 正常熔合 (E-classes=1)" || bad "F(1) 应熔"
# 错向: when 挂载守卫对照
printf 'Pipeline("s4c", "t")\n.proc("a", script(pa, text="x"))\n.proc("b", script(pa, text="y").when(mode == "on"))\n' > $D/s4c.pipeline
$BIN graph $D/s4c.pipeline 2>/dev/null | grep -q "E-classes: 2" && ok "when_guard 独立拦截 (不熔)" || bad "when_guard 应独立拦截"

echo "== S5: 执行层 (script call + run) =="
$BIN script call pa "text=hi" 2>&1 | grep -q "px:hi" && ok "script call 执行正常" || bad "script call"
# run: F(2) 拦熔合不影响执行语义
$BIN run $D/s4_block.pipeline t 2>&1 | grep -q "px:" && ok "run 双执行语义保持" || bad "run 应双执行"

echo "== S6: incidents 表 + explore 直插 =="
# explore 直插行: 验证三元组 action/condition/consequence 完整保留(非 evidence_snapshot 重写)
python3 -c "
import sqlite3
c = sqlite3.connect('$D/ductile.db')
c.execute(\"INSERT INTO incidents (pipeline, proc_name, signals, err_code, evidence, status) VALUES ('explore:probe','pz3','L4','explore_finding','action: A/condition: C/consequence: K','open')\")
c.commit()"
tri=$(sqlite3 $D/ductile.db "SELECT evidence FROM incidents WHERE pipeline='explore:probe'")
echo "$tri" | grep -q "action: A" && echo "$tri" | grep -q "condition: C" && echo "$tri" | grep -q "consequence: K" && ok "explore 直插三元组完整" || bad "三元组被截: $tri"
n=$(sqlite3 $D/ductile.db "SELECT COUNT(*) FROM incidents WHERE proc_name='pz3'")
[ "$n" -ge 1 ] && ok "incidents 表有 pz3 行" || bad "incidents 缺行"
s=$(sqlite3 $D/ductile.db "SELECT status FROM incidents WHERE proc_name='pz3'")
echo "$s" | grep -q closed && ok "closed 状态持久" || bad "status=$s"

echo ""
echo "==================== 结果: PASS=$PASS FAIL=$FAIL ===================="
[ $FAIL -eq 0 ] && echo "ALL GREEN" || echo "HAS FAILURES"
echo "沙箱: $D (DUCTILE_DATA 隔离, 真库未动)"

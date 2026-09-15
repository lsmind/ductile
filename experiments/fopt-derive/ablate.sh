#!/usr/bin/env bash
# FOPT 派生刀对比测试组（消融: old=8345d4f 无派生 / new=派生刀）
# 判据全确定性: script show 的 cse_safe 判定行 —— 零 LLM。
set -euo pipefail
cd "$(dirname "$0")"

OLD=${FOPT_OLD:-/tmp/fopt_old_bin}
NEW=${FOPT_NEW:-/tmp/fopt_new_bin}
D=build_abl
rm -rf $D && mkdir -p $D

cat > $D/px.sh <<'EOF'
# ductile: v1
# name: px
# lang: bash
# desc: pure probe
# params: text(str, required)
# output: echo(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(1)-O(1)-P(1)-T(1)
echo "px:$DUCTILE_ARG_TEXT"
EOF

run_case() {
    local BIN=$1 TAG=$2 INCIDENT=$3
    local DD=$D/data_$TAG
    export DUCTILE_DATA=$PWD/$DD
    mkdir -p $DD
    $BIN script attach $D/px.sh >/dev/null 2>&1
    if [ "$INCIDENT" = "yes" ]; then
        python3 -c "
import sqlite3, os
c = sqlite3.connect(os.path.join(os.environ['DUCTILE_DATA'], 'ductile.db'))
c.execute(\"INSERT INTO incidents (pipeline, proc_name, signals, err_code, evidence, status) VALUES ('script:px','px','L0','explore_finding','action: x','open')\")
c.commit()"
    fi
    $BIN script show px > $D/show_$TAG.txt 2>&1 || true
    # 手标收紧为 F(2)（第二态）
    python3 -c "
import sqlite3, os
c = sqlite3.connect(os.path.join(os.environ['DUCTILE_DATA'], 'ductile.db'))
c.execute(\"UPDATE scripts SET mcsm='F(2)-O(1)-P(1)-T(1)' WHERE name='px'\")
c.commit()"
    $BIN script show px >> $D/show_$TAG.txt 2>&1 || true
    # 第三态: incident 关闭后派生应解除(状态机可回稳态)
    if [ "$INCIDENT" = "yes" ]; then
        python3 -c "
import sqlite3, os
c = sqlite3.connect(os.path.join(os.environ['DUCTILE_DATA'], 'ductile.db'))
c.execute(\"UPDATE scripts SET mcsm='F(1)-O(1)-P(1)-T(1)' WHERE name='px'\")
c.execute(\"UPDATE incidents SET status='closed' WHERE proc_name='px'\")
c.commit()"
        $BIN script show px >> $D/show_$TAG.txt 2>&1 || true
    fi
    unset DUCTILE_DATA
}

run_case $OLD old_no  no
run_case $NEW new_no  no
run_case $OLD old_inc yes
run_case $NEW new_inc yes

echo "===== 对比矩阵（两行=手标F(1)/F(2)；inc 格三行=+关闭incident后）====="
for t in old_no new_no old_inc new_inc; do
    echo "--- $t ---"
    grep -i "cse" $D/show_$t.txt | sed 's/^/  /' || echo "  (no cse line)"
done
echo "===== 预期 ====="
echo "old_no/new_no: 两行都 true（无incident 手标无2）"
echo "old_inc: 两行 true（老二进制不读incident）"
echo "new_inc: 第一行 false（open incident→派生(2)→拦）、第二行 false（手标2收紧）、第三行 true（incident关+手标回1→解除）"

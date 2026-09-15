#!/usr/bin/env bash
# FOPT 全链影响对比测试（三臂消融, 判据全确定性零 LLM）
#   v1810 = f2bf73a (v0.18.10, FOPT 引入前——无坐标键/无校验/无承重)
#   v1812 = 577e8bc (v0.18.12, 有坐标键+校验, 但熔合决策未接线)
#   head  = 当前   (v0.18.13 承重 + v0.19.x 派生刀)
# 四判据: A.熔合(graph E-classes/alias) B.cse_safe(手标2/incident派生/无标注)
#        C.执行计数(run px: 行数) D.校验门禁(非法F(9)拒绝)
set -euo pipefail
cd "$(dirname "$0")"

V1810=${FOPT_V1810:-/tmp/fopt_v1810_bin}
V1812=${FOPT_V1812:-/tmp/fopt_old_bin}
HEADB=${FOPT_HEAD:-/tmp/fopt_new_bin}
D=build_foptcmp
rm -rf $D && mkdir -p $D

# 四个脚本(唯一名, 修同名 upsert 覆盖 bug): F(1) / F(2) / 无标注 / 非法F(9)
mkscript() { # $1=file $2=name $3=mcsm行(空=无)
    { echo "# ductile: v1"; echo "# name: $2"; echo "# lang: bash"; echo "# desc: probe"
      echo "# params: text(str, required)"; echo "# output: echo(str)"
      echo "# pure: true"; echo "# idempotent: true"; echo "# concurrency: safe"
      echo "# effects: none"; echo "# timeout: 10"
      [ -n "$3" ] && echo "$3"
      echo 'echo "px:$DUCTILE_ARG_TEXT"'; } > $1
}
mkscript $D/pa.sh pa '# mcsm: F(1)-O(1)-P(1)-T(1)'
mkscript $D/pb.sh pb '# mcsm: F(2)-O(1)-P(1)-T(1)'
mkscript $D/pc.sh pc ''
mkscript $D/pd.sh pd '# mcsm: F(9)-O(1)-P(1)-T(1)'

mkpipe() { printf 'Pipeline("foptcmp", "t")\n.proc("a", script(%s, text="hello"))\n.proc("b", script(%s, text="world"))\n' "$1" "$1" > $2; }

run_arm() {
    local BIN=$1 TAG=$2
    local DD=$D/data_$TAG
    export DUCTILE_DATA=$PWD/$DD
    mkdir -p $DD

    # B: attach+show 三形态 (pa手标1 / pb手标2 / pc无标注)
    for s in pa pb pc; do
        $BIN script attach $D/$s.sh >/dev/null 2>&1 || echo "attach-fail:$s" >> $D/b_$TAG.txt
        $BIN script show $s 2>/dev/null | grep -i "cse_safe" | sed "s/^/$s /" >> $D/b_$TAG.txt || echo "$s no-cse-line" >> $D/b_$TAG.txt
    done

    # B+: 派生刀(仅 head 有): 给 pb2 造 open incident 后看 pa(手标1)派生
    sed 's/# name: pa/# name: pz/' $D/pa.sh > $D/pz.sh
    $BIN script attach $D/pz.sh >/dev/null 2>&1
    python3 -c "
import sqlite3, os
c = sqlite3.connect(os.path.join(os.environ['DUCTILE_DATA'], 'ductile.db'))
c.execute(\"INSERT INTO incidents (pipeline, proc_name, signals, err_code, evidence, status) VALUES ('x','pz','L0','explore_finding','a','open')\")
c.commit()"
    $BIN script show pz 2>/dev/null | grep -i "cse_safe" | sed "s/^/pz+incident /" >> $D/b_$TAG.txt || echo "pz no-cse-line" >> $D/b_$TAG.txt

    # A: 熔合 (同构对: 同脚本两个 proc)
    mkpipe pa $D/fa_$TAG.pipeline
    mkpipe pb $D/fb_$TAG.pipeline
    for f in fa fb; do
        $BIN graph $D/${f}_$TAG.pipeline 2>/dev/null | grep -E "^(  Nodes|  E-classes|  CSE aliases)" | tr '\n' ' ' | sed "s/^/$f: /" >> $D/a_$TAG.txt
        echo "" >> $D/a_$TAG.txt
    done

    # C: 执行计数 (pb 管线 run——两同构 proc 各跑一次?)
    mkpipe pb $D/r_$TAG.pipeline
    $BIN run $D/r_$TAG.pipeline t > $D/run_$TAG.txt 2>&1 || true
    grep -c "px:" $D/run_$TAG.txt > $D/c_$TAG.txt || echo 0 > $D/c_$TAG.txt
    # C+: 无标注 pc 管线对照
    mkpipe pc $D/r2_$TAG.pipeline
    $BIN run $D/r2_$TAG.pipeline t > $D/run2_$TAG.txt 2>&1 || true
    grep -c "px:" $D/run2_$TAG.txt > $D/c2_$TAG.txt || echo 0 > $D/c2_$TAG.txt

    # D: 非法 F(9)
    $BIN script attach $D/pd.sh > $D/d_$TAG.txt 2>&1 || echo "REJECT" >> $D/d_$TAG.txt
    grep -q "attached: pd" $D/d_$TAG.txt && echo "ACCEPTED(bad!)" >> $D/d_$TAG.txt || true

    unset DUCTILE_DATA
}

run_arm $V1810 v1810
run_arm $V1812 v1812
run_arm $HEADB head

echo "======== A. 熔合(同构 script 对: fa=手标F(1) / fb=手标F(2)) ========"
for t in v1810 v1812 head; do echo "--- $t ---"; sed 's/^/  /' $D/a_$t.txt; done
echo "======== B. cse_safe (pa=手标1 pb=手标2 pc=无 pz=incident派生) ========"
for t in v1810 v1812 head; do echo "--- $t ---"; sed 's/^/  /' $D/b_$t.txt; done
echo "======== C. 执行计数 (px: 行数: c=pb手标2 / c2=pc无标注) ========"
for t in v1810 v1812 head; do echo "  $t: pb=$($D/c_$t.txt 2>/dev/null || cat $D/c_$t.txt) pc=$(cat $D/c2_$t.txt)"; done
echo "======== D. 非法坐标 F(9) ========"
for t in v1810 v1812 head; do echo "--- $t ---"; sed 's/^/  /' $D/d_$t.txt; done

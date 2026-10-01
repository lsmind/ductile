#!/usr/bin/env bash
# Ed25519 信任帧 failpoint 崩溃矩阵（规格 §7.2：五点位×ROTATE/REVOKE；old/new 二择+验签全过）
set -euo pipefail
cd /home/beef/projects/ductile

# failpoint 构建隔离目录（不入默认 target/，防争用）
FPDIR=target/failpoint-auth
if [ ! -x "$FPDIR/release/ductile" ] || [ "$(find src Cargo.toml -newer "$FPDIR/release/ductile" 2>/dev/null | head -1)" != "" ]; then
  cargo build --release --features mlv-failpoint --target-dir "$FPDIR" 2>&1 | tail -1
fi
BIN=$FPDIR/release/ductile

PASS=0; FAIL=0
for KIND in ROTATE REVOKE; do
  for FP in after_write after_sync before_rename after_rename before_dirsync; do
    D=$(mktemp -d)
    L=$D/l.jsonl
    $BIN mlv $L keygen --out $D/k >/dev/null
    S=$(ls $D/k/*.secret)
    $BIN mlv $L init --mode ed25519 --root-key "$S" >/dev/null
    $BIN mlv $L create b 0 --signing-key "$S" >/dev/null
    if [ "$KIND" = ROTATE ]; then
      $BIN mlv $L keygen --out $D/v >/dev/null
      S2=$(ls $D/v/*.secret)
      CMD="rotate --new-key $S2 --signing-key $S"
    else
      F=$(basename "$S"); KID=${F#ed25519-}; KID="${KID%.secret}"
      CMD="revoke --key-id ${KID:0:32} --signing-key $S"
    fi
    # 注入 failpoint（abort=134）
    DUCTILE_MLV_FAILPOINT=$FP $BIN mlv $L $CMD >/dev/null 2>&1 && RC=0 || RC=$?
    if [ "$RC" != "134" ]; then
      echo "BAD-$KIND-$FP: expected abort(134) got rc=$RC"; FAIL=$((FAIL+1)); rm -rf $D; continue
    fi
    # 崩溃后：open 必须成功（old 或 new 二择），verify 全过
    if $BIN mlv $L verify >/dev/null 2>&1; then
      PASS=$((PASS+1))
    else
      echo "BAD-$KIND-$FP: post-crash verify failed"; FAIL=$((FAIL+1))
    fi
    rm -rf $D
  done
done
echo "auth-failpoint-matrix: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ] && [ "$PASS" = 10 ] && echo "AUTH-FAILPOINT-PASS" || exit 1

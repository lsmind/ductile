#!/usr/bin/env bash
# ductile: v1
# name: abl_echo5
# desc: A5消融探针——副作用计数器脚本(每次调用计数+1并落盘), cse_safe=false
# lang: bash
# params: text(str, required), tdir(str, default=/tmp/abl)
# output: value(str)
# pure: false
# idempotent: false
# concurrency: exclusive
# effects: fs
# timeout: 10
set -euo pipefail
TEXT="${DUCTILE_ARG_TEXT:?}"
TD="${DUCTILE_ARG_TDIR:-/tmp/abl}"
mkdir -p "$TD"
exec 9>"$TD/a5.lock"
flock 9
F="$TD/a5_count"
V="$(cat "$F" 2>/dev/null || echo 0)"
V=$((V+1))
printf '%s\n' "$V" > "$F"
echo "##DSL_RESULT"
echo "value=$TEXT"
echo "##DSL_END"

#!/usr/bin/env bash
# ductile: v1
# name: make_report
# desc: 在指定目录生成报告文件（有副作用示例：写文件，幂等）
# lang: bash
# params: dir(path, required), title(str, default=untitled)
# output: file(path), size(int)
# pure: false
# idempotent: true
# concurrency: exclusive
# effects: fs
# timeout: 15
# retries: 1
set -euo pipefail
dir="$DUCTILE_ARG_DIR"
title="$DUCTILE_ARG_TITLE"
mkdir -p "$dir"
out="$dir/report.txt"
{
  echo "title: $title"
  echo "topic: $DUCTILE_TOPIC"
} > "$out"
size=$(wc -c < "$out")
echo "##DSL_RESULT"
echo "file=$out"
echo "size=$size"
echo "##DSL_END"

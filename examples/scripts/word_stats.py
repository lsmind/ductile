#!/usr/bin/env python3
# ductile: v1
# name: word_stats
# desc: 统计文本的字数/行数/最长行（纯函数示例）
# lang: python
# params: text(str, required)
# output: words(int), lines(int), longest(int)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
import os

text = os.environ.get("DUCTILE_ARG_TEXT", "")
lines = [ln for ln in text.splitlines()]
words = sum(len(ln.split()) for ln in lines)
longest = max((len(ln) for ln in lines), default=0)
print("##DSL_RESULT")
print(f"words={words}")
print(f"lines={len(lines)}")
print(f"longest={longest}")
print("##DSL_END")

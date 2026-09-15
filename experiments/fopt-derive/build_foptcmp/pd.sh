# ductile: v1
# name: pd
# lang: bash
# desc: probe
# params: text(str, required)
# output: echo(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(9)-O(1)-P(1)-T(1)
echo "px:$DUCTILE_ARG_TEXT"

# ductile: v1
# name: pz
# lang: bash
# desc: probe
# params: text(str, required)
# output: echo(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(1)-O(1)-P(1)-T(1)
echo "px:$DUCTILE_ARG_TEXT"

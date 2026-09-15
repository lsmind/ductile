# ductile: v1
# name: pc
# lang: bash
# desc: probe
# params: text(str, required)
# output: echo(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
echo "px:$DUCTILE_ARG_TEXT"

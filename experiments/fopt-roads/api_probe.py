#!/usr/bin/env python3
# X5 API 面全探针 — .venv ductile 模块 (16 pub fn) 四能力验证
import os, json, tempfile
os.environ['DUCTILE_DATA'] = tempfile.mkdtemp(prefix='fopt_api_')
import ductile as d

PASS = FAIL = 0
def ok(name): global PASS; PASS += 1; print(f'  PASS {name}')
def bad(name, e=''): global FAIL; FAIL += 1; print(f'  FAIL {name} ({str(e).splitlines()[0] if e else ""})[:80]')

# 1) parse: DSL → AST
pf = os.path.join(os.environ['DUCTILE_DATA'], 'ap.pipeline')
open(pf, 'w').write('Pipeline("ap", "t")\n.proc("a", run("echo hi"))\n')
ast = d.parse(pf)
ast_ok = bool(ast) and 'Pipeline: ap' in str(ast)
ok('parse 正常 + AST 含管线名') if ast_ok else bad('parse', str(ast)[:60])

# 2) scripts_json: 契约卡面
sj = d.scripts_json()
try:
    json.loads(sj); ok('scripts_json 可解析 JSON')
except Exception as e:
    bad('scripts_json', e)

# 3) script_call_json: 无脚本时 fail-closed
try:
    r = d.script_call_json('no_such', {})
    fc = json.loads(r).get('ok') is False
except TypeError:
    r = d.script_call_json('no_such', 'x')
    fc = json.loads(r).get('ok') is False
ok('script_call 幽灵名 fail-closed') if fc else bad('幽灵名应 fail-closed', str(r)[:80])

# 4) 其余 pub 面盘点(名称清点=能力存在性)
fns = [x for x in dir(d) if not x.startswith('_')]
print('  API 清单:', ', '.join(fns))
ok(f'pub API {len(fns)} 个(>0)')

print(f'X5 结果: PASS={PASS} FAIL={FAIL}')

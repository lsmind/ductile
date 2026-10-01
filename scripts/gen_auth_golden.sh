#!/usr/bin/env bash
# golden 向量（确定性子集）：seed→root_key_id/H0 为纯函数输出（无时钟）。
# genesis record_hash 含 accepted_at_ns（wall clock）不入 golden——规格锁公式不锁时序值。
set -euo pipefail
cd /home/beef/projects/ductile
mkdir -p tests/golden
python3 - <<'PYEOF'
import subprocess, tempfile, os, re

seed = bytes.fromhex('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60')
d = tempfile.mkdtemp()
kp = os.path.join(d, 'k.secret')
open(kp, 'wb').write(seed)
os.chmod(kp, 0o600)
L = os.path.join(d, 'l.jsonl')
BIN = 'target/release/ductile'
r = subprocess.run([BIN, 'mlv', L, 'init', '--mode', 'ed25519', '--root-key', kp],
                   capture_output=True, text=True, check=True)
root = re.search(r'root=([0-9a-f]{32})', r.stdout).group(1)

data = open(L, 'rb').read()
n = int.from_bytes(data[:8], 'big')
j = data[9:9+n].decode()
# payload 内 auth 对象为转义嵌套（\"trust_hash\":\"sha256:...）——用简单子串锚点提取
import re as _re
h0 = _re.search(r'trust_hash[^s]*sha256:([0-9a-f]+)', j).group(1)
h0 = 'sha256:' + h0
pk_hex = _re.search(r'root_public_key[^0-9a-f]*([0-9a-f]{64})', j).group(1)

open('tests/golden/mlv_auth_v1.txt', 'w').write(
    '# MLV auth golden vector v1 (RFC 8032 TEST 1 seed; frozen spec v1.1)\n'
    '# 确定性子集：root_key_id/H0 为纯函数；genesis hash 含 wall-clock 不入锁\n'
    f'seed={seed.hex()}\n'
    f'root_public_key={pk_hex}\n'
    f'root_key_id={root}\n'
    f'h0={h0}\n'
    'rfc8032_sig=e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b\n'
)
print('golden written: root=' + root)
PYEOF
cat tests/golden/mlv_auth_v1.txt

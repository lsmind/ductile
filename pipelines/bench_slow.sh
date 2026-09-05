#!/bin/bash
# v0.11 measure 验收脚本：sleep 0.2s 后输出实测延迟（秒，纯小数——stdout 第一个浮点数被引擎取走）
python3 -c "import subprocess,time; t=time.time(); subprocess.run(['sleep','0.2']); print(f'{time.time()-t:.3f}')"

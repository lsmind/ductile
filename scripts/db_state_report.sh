#!/usr/bin/env bash
# db_state_report.sh — 清账后状态报告（P3 deliver）。
# 引号策略：SQL 内嵌单引号在 DSL 双引号里会被剥，所以整个查询落在本脚本里。
set -euo pipefail
DB="/home/beef/.local/share/ductile/ductile.db"
export DUCTILE_DATA="/home/beef/.local/share/ductile"

/home/beef/projects/ductile/target/release/ductile script doctor 2>&1 | tail -3 || true
echo ---
sqlite3 "$DB" "SELECT
  (SELECT COUNT(*) FROM incidents WHERE status='open')  AS open_incidents,
  (SELECT COUNT(*) FROM hyper_graphs)                   AS hyper_graphs,
  (SELECT COUNT(*) FROM pipelines WHERE source_file != '') AS registered,
  (SELECT COUNT(*) FROM (SELECT DISTINCT pipeline FROM canary_runs)) AS canary_pipelines"
echo ---
echo "degraded flags: $(ls /home/beef/.local/share/ductile/degraded/ 2>/dev/null | wc -l)"

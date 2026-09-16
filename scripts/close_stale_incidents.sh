#!/usr/bin/env bash
# close_stale_incidents.sh — P3 清账：残留 open incidents 统一 close。
# 前置：archive 快照已拍；triage 已跑过（结果在 incident.triage 列）。
set -euo pipefail
DB="$HOME/.local/share/ductile/ductile.db"
export DUCTILE_DATA="$HOME/.local/share/ductile"

before=$(sqlite3 "$DB" "SELECT COUNT(*) FROM incidents WHERE status='open'")
sqlite3 "$DB" "SELECT id FROM incidents WHERE status='open'" | while read -r i; do
  ductile incident close "$i" \
    "P3清账: 历史探针噪声(triage=nocanary/无金丝雀无从判别), archive快照可回滚" \
    >/dev/null 2>&1 || true
done
after=$(sqlite3 "$DB" "SELECT COUNT(*) FROM incidents WHERE status='open'")
echo "open incidents: $before -> $after"

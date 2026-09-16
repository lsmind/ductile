CREATE TABLE IF NOT EXISTS canaries (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline  TEXT NOT NULL,
    proc_name TEXT NOT NULL,
    input     TEXT NOT NULL,
    expect    TEXT NOT NULL DEFAULT '@self.ok == 1',
    note      TEXT DEFAULT '',
    saved_at  TEXT DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_canaries_proc ON canaries(proc_name);
CREATE TABLE IF NOT EXISTS canary_runs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline  TEXT NOT NULL,
    proc_name TEXT NOT NULL,
    pass      INTEGER NOT NULL,
    detail    TEXT DEFAULT '',
    ran_at    TEXT DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_canary_runs_proc ON canary_runs(pipeline, proc_name);
-- X3 矛盾态禁播测试需要 incidents 表（真实库由主 SCHEMA_DDL 建，测试库在此镜像最小列集）
CREATE TABLE IF NOT EXISTS incidents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline    TEXT NOT NULL,
    proc_name   TEXT NOT NULL,
    signals     TEXT DEFAULT '',
    err_code    TEXT DEFAULT '',
    evidence    TEXT DEFAULT '',
    status      TEXT DEFAULT 'open',
    created_at  TEXT DEFAULT ''
);

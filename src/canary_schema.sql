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

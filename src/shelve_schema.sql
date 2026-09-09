CREATE TABLE IF NOT EXISTS shelved (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline    TEXT NOT NULL,
    proc_name   TEXT NOT NULL,
    reason      TEXT DEFAULT '',
    evidence    TEXT DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'open',
    resolution  TEXT DEFAULT '',
    created_at  TEXT DEFAULT '',
    resolved_at TEXT DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_shelved_status ON shelved(status);

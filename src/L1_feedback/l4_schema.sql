CREATE TABLE IF NOT EXISTS l4_reviews (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline    TEXT NOT NULL,
    verdict     TEXT NOT NULL,
    evidence    TEXT DEFAULT '',
    label       TEXT NOT NULL DEFAULT '',
    run_id      INTEGER,
    reviewed_at TEXT DEFAULT '',
    label_source TEXT DEFAULT 'human'
);
CREATE INDEX IF NOT EXISTS idx_l4_reviews_pipeline ON l4_reviews(pipeline);

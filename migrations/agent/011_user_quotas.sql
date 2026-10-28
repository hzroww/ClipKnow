-- Lifetime free questions. Existing conversations are deliberately not backfilled.
CREATE TABLE user_quotas (
    user_id TEXT PRIMARY KEY REFERENCES users(id),
    free_limit INTEGER NOT NULL DEFAULT 5 CHECK (free_limit >= 0),
    free_used INTEGER NOT NULL DEFAULT 0 CHECK (free_used >= 0 AND free_used <= free_limit),
    updated_at INTEGER NOT NULL
);

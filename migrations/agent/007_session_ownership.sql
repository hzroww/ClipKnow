-- 会话归属。**Rust 拥有 sessions / turns / items**，Go 运行时不读写。

-- ── 会话归属 ─────────────────────────────────────────────
-- user_id 先允许 NULL，导入脚本填完之后再由 008 收紧成 NOT NULL。
-- SQLite 的 ALTER TABLE ADD COLUMN 加不了 NOT NULL（除非给默认值），
-- 而这一列没有合理的默认值——「属于谁」不能猜。
ALTER TABLE sessions ADD COLUMN user_id TEXT REFERENCES users(id) ON DELETE RESTRICT;
ALTER TABLE sessions ADD COLUMN updated_at INTEGER;
ALTER TABLE sessions ADD COLUMN deleted_at INTEGER;
-- 创建会话的幂等键：同一个 (user_id, creation_request_id) 重试只建一个会话
ALTER TABLE sessions ADD COLUMN creation_request_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS uq_session_creation_request
    ON sessions(user_id, creation_request_id)
    WHERE creation_request_id IS NOT NULL;

-- 列表查询的形状：先按用户过滤，再按更新时间分页。
-- 抄的是 Laplace 的 idx_conversations_user_last_activity。
CREATE INDEX IF NOT EXISTS idx_sessions_user_updated
    ON sessions(user_id, updated_at DESC, id DESC)
    WHERE deleted_at IS NULL;

-- 多用户第一版：账号、登录态、会话归属。
--
-- 表的拥有权（见设计文档第 5 节）：
--   users / auth_sessions   Go 拥有，Rust 运行时不读写
--   sessions / turns        Rust 拥有，Go 运行时不读写
-- 共用一个数据库只是部署选择，不是跨语言直接读对方表的理由。

-- ── 账号 ─────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS users (
    id                  TEXT PRIMARY KEY,   -- 稳定 UUID，业务表只认它
    -- 规范化后的用户名（小写）。唯一性靠这一列的约束，不靠先查后插——
    -- 并发注册时「查一下没有、再插入」两步之间有竞态窗口。
    username_normalized TEXT NOT NULL UNIQUE,
    -- 用户自己写的大小写，只用于显示
    display_name        TEXT NOT NULL,
    -- Argon2id 的完整编码串（含算法、参数、salt），不是裸哈希。
    -- 参数以后要调时，老用户凭串里的旧参数仍能验证通过。
    password_hash       TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'active',
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (status IN ('active', 'disabled'))
);

-- ── 登录态 ───────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS auth_sessions (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- ★ 只存哈希，不存令牌本身。库被读走时，里面的东西不能直接拿来登录。
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_auth_sessions_user ON auth_sessions(user_id);
-- 清理过期记录时按这个扫
CREATE INDEX IF NOT EXISTS idx_auth_sessions_expires ON auth_sessions(expires_at);

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

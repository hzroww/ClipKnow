-- 账号与登录态。**Go 拥有这两张表**，Rust 运行时不读写。
--
-- 放在 migrations/accounts/ 下是为了让拥有权在目录结构上就看得见：
-- 共用一个数据库只是部署选择，不是跨语言直接读对方表的理由。
-- 执行仍由同一个 runner（clipknow migrate）按全局版本号顺序跑。

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


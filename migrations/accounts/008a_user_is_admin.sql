-- 管理员标记。Go 拥有。
--
-- 007a 漏了这一列——邀请码那套里 admin 是存在 access.json 的，建表时没想起来
-- 它也要搬过来。按「已有迁移不回改，只往后加」的规矩单起一条。
ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;

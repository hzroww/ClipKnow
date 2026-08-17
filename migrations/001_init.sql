-- ClipKnow 初始表结构
--
-- SQLite 不是数据库服务器，就是一个文件。这些表全都躺在 clipknow.db 里。
-- 备份 = 复制这个文件。

-- 1. 视频本体
CREATE TABLE IF NOT EXISTS videos (
  id            TEXT PRIMARY KEY,   -- 我们自己生成的 uuid v7
  platform      TEXT NOT NULL,      -- youtube / tiktok / instagram
  native_id     TEXT NOT NULL,      -- 视频在平台上的原始 ID
  url           TEXT NOT NULL,
  title         TEXT,
  author_handle TEXT,
  author_name   TEXT,
  duration_sec  INTEGER,
  published_at  INTEGER,            -- unix 时间戳（秒）
  view_count    INTEGER,
  like_count    INTEGER,
  comment_count INTEGER,
  description   TEXT,
  raw_json      TEXT NOT NULL,      -- SC 返回的原始 JSON，整个存下来
  fetched_at    INTEGER NOT NULL,
  UNIQUE(platform, native_id)       -- 同一个视频不会有两条记录
);

-- 2. 文字稿。一个视频一条，所以 video_id 直接当主键。
CREATE TABLE IF NOT EXISTS transcripts (
  video_id   TEXT PRIMARY KEY REFERENCES videos(id) ON DELETE CASCADE,
  text       TEXT NOT NULL,
  source     TEXT NOT NULL,         -- 'sc'，以后自建语音识别时是 'asr'
  lang       TEXT,
  fetched_at INTEGER NOT NULL
);

-- 3. 评论
CREATE TABLE IF NOT EXISTS comments (
  id           TEXT PRIMARY KEY,
  video_id     TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
  author       TEXT,
  text         TEXT NOT NULL,
  like_count   INTEGER,
  published_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_comments_video ON comments(video_id);

-- 3.5 抓取产物（artifacts）
--
-- 一次摄取会调三个端点（详情/文字稿/评论），每个都可能有三种结局。
-- 不区分的话会出两类问题：
--   a) 「调用失败」被当成「确实没有」，永久缓存下来，以后再也不会补抓
--   b) --refresh 时评论请求失败，旧评论被清空 → 新元数据 + 空评论的混合状态
-- 所以每个端点单独记状态，并把**原始响应**存下来。
--
-- status:
--   ok          — 拿到内容了
--   unavailable — 调用成功，但确实没有（纯画面视频没人声、视频没评论）
--   failed      — 调用失败（网络错误、SC 报错、AI 转录重试后仍失败）
--
-- raw_json 存每个端点的原始响应。videos.raw_json 只有详情那一份，
-- 文字稿和评论的原始数据以前是丢掉的——所以「解析漏字段随时能补」
-- 这个承诺之前并不成立（改 Instagram transcripts[] 时只能付费重抓）。
CREATE TABLE IF NOT EXISTS artifacts (
  video_id   TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL,      -- detail / transcript / comments
  status     TEXT NOT NULL,      -- ok / unavailable / failed
  raw_json   TEXT,               -- 原始响应；failed 且没拿到响应时为 NULL
  error      TEXT,               -- failed 时的错误信息
  fetched_at INTEGER NOT NULL,
  PRIMARY KEY (video_id, kind)
);

-- 4/5. 会话和消息。
-- 第一版一问一答、答完就退出，不往这两张表写东西。
-- 但表先建好：第二步加 agent 循环时立刻要用，到时候不用改表结构、不用迁数据。
CREATE TABLE IF NOT EXISTS sessions (
  id         TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  title      TEXT
);

CREATE TABLE IF NOT EXISTS messages (
  id         TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq        INTEGER NOT NULL,      -- 第几条，保证顺序
  role       TEXT NOT NULL,         -- user / assistant / tool
  content    TEXT NOT NULL,         -- 存 JSON 而非纯文本：第二步一条 assistant 消息
                                    -- 可能同时含文字和「我要调用工具 X」，纯文本装不下
  created_at INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);

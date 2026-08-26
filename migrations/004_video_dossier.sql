-- 视觉档案：一条视频的画面被看过之后留下的结构化结论。
--
-- 为什么建新表，而不是给 videos 加几列：
--   1. videos 已经十几列，再加四列会更难读
--   2. 一对多是**刻意的**——换模型或调 fps 后重新分析，旧档案留着能对比，
--      不然没法判断新模型是不是真的更好
--   3. 和 transcripts / comments 挂 video_id 的模式一致
--
-- **视频字节不落库。** 存的是档案（几百字符），不是视频（几十 MB）。
-- 档案一旦生成，视频就没用了：答案引用的是档案里的结论。视频真被平台
-- 删了，档案反而是唯一留下来的东西。

CREATE TABLE IF NOT EXISTS video_dossiers (
  id           TEXT PRIMARY KEY,
  video_id     TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,

  -- 结构化档案的 JSON。模型没按格式输出时存兜底的原文——
  -- 分析已经花掉了，丢弃等于白花。
  dossier_json TEXT NOT NULL,

  -- 生成时用的模型和抽帧率。两个都要记：
  --   model 让你在换模型后认出哪些档案是老的
  --   fps   是这份档案的**分辨率**，要写进给模型看的状态行
  model        TEXT NOT NULL,
  fps          REAL NOT NULL,

  -- NULL = 通用档案，读取时取最新那条
  -- 有值 = 带具体问题看的结果
  --
  -- **追加不覆盖。** 因为不存视频，每次带问题重看都要重新下载 + 重新分析
  -- （≈ 一次完整分析的成本）。把问过的答案留着、附在档案后面给模型看，
  -- 它见过就不会重复花钱再问一遍同一个细节。
  question     TEXT,

  -- 视觉模型报回的真实 video token 数（DashScope 在
  -- prompt_tokens_details.video_tokens 里单独给）。用来对账，不靠估算。
  video_tokens INTEGER,

  created_at   INTEGER NOT NULL
);

-- 读档案永远是「这个视频最新的」，所以按 (video_id, created_at DESC) 建索引
CREATE INDEX IF NOT EXISTS idx_dossier_video
  ON video_dossiers(video_id, created_at DESC);

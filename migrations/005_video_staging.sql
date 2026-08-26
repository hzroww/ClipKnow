-- 视频上传引用：把视频放到视觉模型取得到的地方之后，记下那个引用。
--
-- 为什么加在 video_dossiers 上而不是建新表：
--   一条视频**同时最多一个有效上传**，而档案可以有多份。看着像是该分两张
--   表，但两张表也一样防不住下面那个坑（代码照样能写错查询），真正防住它
--   的是列名 + 回归测试。而 ClipKnow 一直的原则是够用就好——上一次做压缩
--   时同样的取舍，选了「挂在 turns 上加两列」而不是建表。
--
-- 代价两条，都能接受：
--   1. 一条视频上传一次、分析五次，那五行会重复存同一个引用（约 100 字符）
--   2. 「上传成功但分析失败」不会被记下来，下次要重传。stage 成功说明文件
--      已经在服务端了，紧接着 analyze 失败是罕见情况；真常发生了再加一列
--      status 就行

ALTER TABLE video_dossiers ADD COLUMN provider TEXT;

-- 厂商特定的引用：百炼是 oss://dashscope-instant/…，换 Gemini 就是 files/…。
-- **绝不给模型或用户看**，它是内部标识。
ALTER TABLE video_dossiers ADD COLUMN staged_ref TEXT;

-- 上传引用什么时候失效。百炼的临时存储空间是 48 小时。
--
-- ⚠️ 列名故意叫 staged_expires_at 而不是 expires_at：它是**上传的过期**，
-- 不是档案的过期。**档案永久有效。**
--
-- 参照的实现在这里踩过坑——把上传过期当成了档案过期，结果每 48 小时白白
-- 重新分析一遍。他们后来专门加了一条列注释纠正：
--   "provider registration expiry. Does not control dossier reuse."
-- 这里用列名把这件事说在明处，再加一条回归测试钉住。
ALTER TABLE video_dossiers ADD COLUMN staged_expires_at INTEGER;

CREATE INDEX IF NOT EXISTS idx_dossier_staged
  ON video_dossiers(video_id, provider, staged_expires_at DESC);

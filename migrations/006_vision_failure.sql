-- 记住分析失败，并区分「永远会失败」和「值得重试」。
--
-- 起因是一次真实会话：一条政治评论视频被视觉模型的内容审查拒了
--   InternalError.Algo.DataInspectionFailed:
--   Input video data may contain inappropriate content.
-- 这个失败是**确定性的**——同一条视频每次都会被同样的审查拦下。而原来的
-- 实现只在分析成功时写档案，所以什么都没记住：下次再问同一条视频，又是
-- 一遍下载 + 上传 + 被拒。
--
-- 两类失败要分开处置：
--
--   确定性（内容审查、格式不支持、太大）
--     → blocked_reason 写原因。以后读到它直接返回，零下载零上传零分析。
--
--   可重试（限流、超时、5xx）
--     → blocked_reason 留空，但 staged_ref + staged_expires_at 照写。
--       上传已经成功了，引用留着下次直接重试 analyze，不用重新下载上传。
--
-- 这和 SC 那边的 is_transient() 是同一个思路：区分「重试一百次都是同样
-- 失败」和「第二次就能成」。

-- 分析被永久性地拒了，原因写在这里。NULL = 没被拒（成功，或只是可重试的失败）。
ALTER TABLE video_dossiers ADD COLUMN blocked_reason TEXT;

-- 失败时写的那一行，dossier_json 是空串。读档案的查询要过滤掉它，
-- 不然会把一份空档案当成真档案返回。
CREATE INDEX IF NOT EXISTS idx_dossier_blocked
  ON video_dossiers(video_id, provider, created_at DESC)
  WHERE blocked_reason IS NOT NULL;

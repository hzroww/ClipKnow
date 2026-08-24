-- 上下文压缩：摘要挂在 turns 上，不建新表。
--
-- 参照的那个实现建过一张 model_context_compactions 表，两天后就 DROP 了，
-- 改成「摘要存在终态响应的 metadata 里」。这里学它的结论：
--   摘要只是**模型线**的产物，而 items 是**存档线**，两条线本来就分开。
--   为一个附属产物建表，还要处理它和 items 的引用完整性，不值得。
--
-- 关键约束：**items 一个字都不改。** 摘要只记录「覆盖到哪个 seq」，
-- 读历史时用它替换那些 turn；原始条目永远原样躺在库里。
-- 这样压缩是可回退的（把两列清空就恢复原状），也不会破坏存档的忠实性。

ALTER TABLE turns ADD COLUMN summary TEXT;
-- 这个摘要覆盖到哪个 turn.seq（含）。和 summary 一起为空表示这个 turn 没压缩过。
ALTER TABLE turns ADD COLUMN summarized_upto INTEGER;

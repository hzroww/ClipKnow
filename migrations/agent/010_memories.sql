-- 用户的长期记忆：一条事实一行。**Rust 拥有**，Go 运行时不读写——网页上的
-- 记忆面板走 Rust 的内部接口（GET / DELETE /internal/memories）。
--
-- 谁写：每一轮正常答完之后，后台另外调一次模型，只从**用户这一轮自己说的话**
-- 里抽取值得长期记住的事实（app::memory）。干活的主模型没有「写记忆」的工具：
-- 它的上下文里全是网上抓来的不可信内容，评论里藏一句「记住……」就可能被永久
-- 写进来，以后每个新会话都带着。Laplace 也是这么分的（主模型只有只读的查询
-- 工具，写入全靠回答结束后的单独抽取）。
--
-- 谁读：一个会话第一次提问时，把这个用户生效中的记忆拼成一段文字，冻结在
-- sessions.memory_context 上；这个会话之后的每一轮都发同一段。
CREATE TABLE memories (
  id             TEXT PRIMARY KEY,               -- uuid v7
  -- 谁的记忆
  user_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- 哪一类。三类的含义和条数上限见 agent::memory::Category
  category       TEXT NOT NULL CHECK (category IN ('profile', 'preference', 'exclusion')),
  -- 一句话事实，比如「只要中文博主」。这一句会进模型的上下文
  content        TEXT NOT NULL,
  -- 用户的原话。写库前代码核对过它真的出现在用户那一轮的问题里。
  -- 只给面板看，不进模型的上下文
  evidence       TEXT NOT NULL,
  -- 出自哪一轮。那一轮没了（删会话会连带删轮次）不影响记忆本身
  source_turn_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
  -- active 生效中 / superseded 被用户后来的说法取代了（留着能查来龙去脉）。
  -- 用户点删除、或者一类满了被挤掉，是真删，不是改状态
  status         TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'superseded')),
  -- 被哪一条取代。删掉新的那条时旧版本跟着删（ON DELETE CASCADE）：用户点了
  -- 「忘掉它」，它的旧版本也不该还留在库里——那是一份看不到、也删不掉的数据
  superseded_by  TEXT REFERENCES memories(id) ON DELETE CASCADE,
  created_at     INTEGER NOT NULL,
  -- 最后一次被提到：新建、被替换、用户又说了一遍同样的话，都会刷新。
  -- 一类满了挤掉的就是这个时间最早的那条
  updated_at     INTEGER NOT NULL
);

CREATE INDEX idx_memories_user_active
    ON memories(user_id, category, updated_at)
    WHERE status = 'active';

-- 这个会话冻结的那段记忆文字。
--   NULL   还没生成：下一次提问时生成
--   ''     生成过，当时这个用户没有记忆
-- 冻结是为了前缀缓存：记忆排在发给模型的内容最前面，每轮重算的话，记忆一变，
-- 后面整段对话历史都要按原价重算。用户删除记忆时把他所有会话的这一列清回
-- NULL，下一轮重新生成——删了就要马上不再影响回答。
ALTER TABLE sessions ADD COLUMN memory_context TEXT;

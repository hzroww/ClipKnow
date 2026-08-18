-- 第二版：agent 循环的会话存储。
--
-- 三层结构：
--   sessions  一次会话（打开到关掉）
--   turns     一次提问（你说一句话 → 它给一个答案）
--   items     这次提问过程中产生的所有条目
--
-- 关键决策：工具调用**不单独建表**，它就是 items 里 item_type 不同的条目。
-- 这样只有一套编号（idx），消息和工具调用天然对得上，不用两张表互相引用。

-- 第一版建了但一直空着的 messages 表：这一版改用 items，语义变了（不只是消息），
-- 名字也换掉。它 0 行，没有迁移代价。
DROP TABLE IF EXISTS messages;

CREATE TABLE IF NOT EXISTS turns (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL,      -- 会话内第几次提问
  model       TEXT NOT NULL,         -- 以后对比 deepseek / claude 时要知道用的谁
  status      TEXT NOT NULL,         -- done / failed，对应状态机的两个终态
  created_at  INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);

CREATE TABLE IF NOT EXISTS items (
  id            TEXT PRIMARY KEY,
  turn_id       TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
  idx           INTEGER NOT NULL,    -- turn 内顺序，重建历史靠它
  item_type     TEXT NOT NULL,       -- user_message / assistant_message
                                     -- / function_call / function_call_output
  iteration     INTEGER,             -- 第几轮模型迭代；user_message 为 NULL
  call_id       TEXT,                -- 配对用；只有两种工具条目有
                                     -- 不加唯一约束：它是模型生成的，全局唯一是
                                     -- 它的实现细节而不是契约
  payload_json  TEXT NOT NULL,       -- **当时实际发给模型的那段文本** + 少量元信息
  raw_json      TEXT,                -- SC 原始响应；只有 function_call_output 有
                                     -- 单独一列而不是塞进 payload：重建历史每轮都要
                                     -- 读 payload，2MB 的原始数据混在里面会白读一遍
  created_at    INTEGER NOT NULL,
  UNIQUE(turn_id, idx)
);

CREATE INDEX IF NOT EXISTS idx_items_turn ON items(turn_id);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);

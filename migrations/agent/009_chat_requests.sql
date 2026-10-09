-- 一次「点发送」一行：请求幂等。**Rust 拥有**，Go 运行时不读写。
--
-- 浏览器点发送时生成一个编号（request_id），重试时带同一个。服务端按
-- (user_id, request_id) 认出「这是同一次提交」：最多建一个会话、最多跑一轮。
--
-- 为什么单独一张表，不加在 sessions / turns 上：一次点发送在服务端可能变成
-- 两件事（新会话的话先建会话，再跑一轮），一行同时指向两者最直接；被拒的
-- 提交（名额满）也有地方记它用的是哪个会话。Laplace 也是这么放的——它的
-- conversations / responses 表上没有幂等键，键都在单独的请求记录表上
-- （queued_response_requests 等），按用户划分。
CREATE TABLE chat_requests (
  -- 谁点的发送。去重按用户划分：别人拿同一个编号查不到你的东西
  user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
  -- 浏览器生成的编号
  request_id  TEXT NOT NULL,
  -- 这次提交落在哪个会话：新会话就是这次建出来的，老会话就是浏览器指定的
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  -- 这次提交建出来的那一轮。被拒（名额满、会话忙）时为空，重试成功后填上。
  -- 没有状态列：跑没跑完看 turns.status，这里再存一份迟早对不上
  turn_id     TEXT REFERENCES turns(id) ON DELETE CASCADE,
  -- 问题原文（去掉首尾空白）。同一个编号来了不同的问题 = 前端 bug，要报错，
  -- 不能悄悄返回另一个问题的那一轮。被拒的提交没有 turn_id，只能拿这一列比对
  question    TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (user_id, request_id)
);

-- 一轮最多属于一次提交
CREATE UNIQUE INDEX uq_chat_requests_turn
    ON chat_requests(turn_id)
    WHERE turn_id IS NOT NULL;

-- sessions.creation_request_id 是多用户那次（007）按设计文档加的，代码从来
-- 没写过它，库里全是 NULL。上面这张表接管了这件事，留着它只会让人以为
-- 它也在管去重。先删依赖它的唯一索引，SQLite 才允许删列。
DROP INDEX IF EXISTS uq_session_creation_request;
ALTER TABLE sessions DROP COLUMN creation_request_id;

//! SQLite 实现。
//!
//! rusqlite 是 SQLite 这个 C 库的 Rust 封装，相当于 C++ 里的 SQLiteCpp。
//! `bundled` feature 让它把 SQLite 源码一起编译进来，所以你的机器上
//! 不需要另外装任何东西。

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::content::model::{
    Artifact, ArtifactKind, Comment, FetchStatus, FetchedVideo, Item, ItemKind, Session,
    Transcript, Turn, TurnStatus, Video, new_id, now_ts,
};
use crate::error::Result;
use crate::ingest::url::Platform;
use crate::store::{Store, StoredVideo};

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// 打开（或创建）一个数据库文件，并建好表。
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// 建一个只存在于内存里的库，测试用——跑完就没了，不会留下垃圾文件。
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        // 外键约束在 SQLite 里默认是关的，要显式打开
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(include_str!("../../migrations/001_init.sql"))?;
        migrate_drop_video_raw_json(&conn)?;
        conn.execute_batch(include_str!("../../migrations/002_agent_loop.sql"))?;
        Ok(Self { conn })
    }
}

/// 迁移：把 `videos.raw_json` 搬进 `artifacts` 表，然后删掉这一列。
///
/// 早期版本只把**详情端点**的原始响应存在 `videos.raw_json`，转录和评论的
/// 直接丢了——所以「解析漏字段无需重抓」的承诺当时只兑现了三分之一。
/// `artifacts` 表统一保存三个端点后，这一列就纯属冗余。
///
/// 幂等：列已经不在就直接返回，所以新建的库跑到这里什么都不做。
/// 搬运用 `INSERT OR IGNORE`，已经有 detail 记录的视频不会被覆盖。
fn migrate_drop_video_raw_json(conn: &Connection) -> Result<()> {
    let has_col = conn
        .prepare("PRAGMA table_info(videos)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == "raw_json");

    if !has_col {
        return Ok(());
    }

    // 先把数据搬走再删列，别把旧库里唯一一份原始响应弄丢了
    conn.execute(
        "INSERT OR IGNORE INTO artifacts (video_id, kind, status, raw_json, error, fetched_at)
         SELECT id, 'detail', 'ok', raw_json, NULL, fetched_at
         FROM videos WHERE raw_json IS NOT NULL AND raw_json != ''",
        [],
    )?;
    conn.execute("ALTER TABLE videos DROP COLUMN raw_json", [])?;
    Ok(())
}

impl Store for SqliteStore {
    fn find_by_native(&self, platform: Platform, native_id: &str) -> Result<Option<StoredVideo>> {
        let video = self
            .conn
            .query_row(
                &format!("SELECT {VIDEO_COLS} FROM videos WHERE platform = ?1 AND native_id = ?2"),
                params![platform.as_str(), native_id],
                row_to_video,
            )
            .optional()?;

        let Some(video) = video else {
            return Ok(None);
        };

        let transcript = self
            .conn
            .query_row(
                "SELECT video_id, text, source, lang, fetched_at FROM transcripts WHERE video_id = ?1",
                params![video.id],
                |r| {
                    Ok(Transcript {
                        video_id: r.get(0)?,
                        text: r.get(1)?,
                        source: r.get(2)?,
                        lang: r.get(3)?,
                        fetched_at: r.get(4)?,
                    })
                },
            )
            .optional()?;

        let mut stmt = self.conn.prepare(
            "SELECT id, video_id, author, text, like_count, published_at
             FROM comments WHERE video_id = ?1
             ORDER BY like_count DESC NULLS LAST",
        )?;
        let comments = stmt
            .query_map(params![video.id], |r| {
                Ok(Comment {
                    id: r.get(0)?,
                    video_id: r.get(1)?,
                    author: r.get(2)?,
                    text: r.get(3)?,
                    like_count: r.get(4)?,
                    published_at: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(Some(StoredVideo {
            video,
            transcript,
            comments,
        }))
    }

    fn save(&mut self, fetched: &FetchedVideo) -> Result<String> {
        // 用事务：要么三张表全写成功，要么一张都不写。
        // 否则可能出现「有视频没文字稿」的半截数据。
        let tx = self.conn.transaction()?;
        let v = &fetched.video;

        // 已经抓过就复用原来的 id，避免外键指向一个新 id 而老数据还挂在旧 id 上
        let existing_id: Option<String> = tx
            .query_row(
                "SELECT id FROM videos WHERE platform = ?1 AND native_id = ?2",
                params![v.platform.as_str(), v.native_id],
                |r| r.get(0),
            )
            .optional()?;
        let video_id = existing_id.unwrap_or_else(|| v.id.clone());

        tx.execute(
            "INSERT INTO videos (id, platform, native_id, url, title, author_handle, author_name,
                                 duration_sec, published_at, view_count, like_count, comment_count,
                                 description, fetched_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(platform, native_id) DO UPDATE SET
                url=excluded.url, title=excluded.title,
                author_handle=excluded.author_handle, author_name=excluded.author_name,
                duration_sec=excluded.duration_sec, published_at=excluded.published_at,
                view_count=excluded.view_count, like_count=excluded.like_count,
                comment_count=excluded.comment_count, description=excluded.description,
                fetched_at=excluded.fetched_at",
            params![
                video_id,
                v.platform.as_str(),
                v.native_id,
                v.url,
                v.title,
                v.author_handle,
                v.author_name,
                v.duration_sec,
                v.published_at,
                v.view_count,
                v.like_count,
                v.comment_count,
                v.description,
                v.fetched_at
            ],
        )?;

        // ★ 文字稿：只有「结论明确」时才动旧数据。
        // 抓取失败（网络错误 / AI 转录挂了）时保持原样——否则 --refresh
        // 碰上一次抖动，就把上次抓好的内容弄丢了。
        if fetched.status_of(ArtifactKind::Transcript).is_conclusive() {
            tx.execute(
                "DELETE FROM transcripts WHERE video_id = ?1",
                params![video_id],
            )?;
            if let Some(t) = &fetched.transcript {
                tx.execute(
                    "INSERT INTO transcripts (video_id, text, source, lang, fetched_at)
                     VALUES (?1,?2,?3,?4,?5)",
                    params![video_id, t.text, t.source, t.lang, t.fetched_at],
                )?;
            }
        }

        // ★ 评论：同样的规则。以前是无条件先删后插，评论请求一失败
        // 就把旧评论全清空了。
        if fetched.status_of(ArtifactKind::Comments).is_conclusive() {
            tx.execute(
                "DELETE FROM comments WHERE video_id = ?1",
                params![video_id],
            )?;
            for c in &fetched.comments {
                tx.execute(
                    "INSERT OR REPLACE INTO comments (id, video_id, author, text, like_count, published_at)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![c.id, video_id, c.author, c.text, c.like_count, c.published_at],
                )?;
            }
        }

        // 三个端点的结局和原始响应都记下来。
        // 原始响应是「以后解析漏了字段能补回来」的唯一依靠——
        // 之前只存了详情那一份，文字稿和评论的原始数据是丢掉的。
        for a in &fetched.artifacts {
            tx.execute(
                "INSERT OR REPLACE INTO artifacts (video_id, kind, status, raw_json, error, fetched_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![video_id, a.kind.as_str(), a.status.as_str(), a.raw_json, a.error, a.fetched_at],
            )?;
        }

        tx.commit()?;
        Ok(video_id)
    }

    fn get_artifacts(&self, video_id: &str) -> Result<Vec<Artifact>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, status, raw_json, error, fetched_at
             FROM artifacts WHERE video_id = ?1 ORDER BY kind",
        )?;
        let rows = stmt
            .query_map(params![video_id], |r| {
                let kind: String = r.get(0)?;
                let status: String = r.get(1)?;
                Ok((
                    kind,
                    status,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .filter_map(|(kind, status, raw_json, error, fetched_at)| {
                // 认不出的 kind 直接跳过：库里被手改过也不该让程序崩
                Some(Artifact {
                    kind: ArtifactKind::from_db(&kind)?,
                    status: FetchStatus::from_db(&status),
                    raw_json,
                    error,
                    fetched_at,
                })
            })
            .collect())
    }

    fn list_videos(&self, limit: usize) -> Result<Vec<Video>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {VIDEO_COLS} FROM videos ORDER BY fetched_at DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_video)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// 列名列一次，SELECT 的地方复用，避免顺序和 `row_to_video` 对不上。
const VIDEO_COLS: &str = "id, platform, native_id, url, title, author_handle, author_name, \
     duration_sec, published_at, view_count, like_count, comment_count, description, fetched_at";

fn row_to_video(r: &Row) -> rusqlite::Result<Video> {
    let platform_str: String = r.get(1)?;
    Ok(Video {
        id: r.get(0)?,
        // 库里的值只可能来自 Platform::as_str()，取不回来说明数据被手改过
        platform: Platform::from_db(&platform_str).unwrap_or(Platform::YouTube),
        native_id: r.get(2)?,
        url: r.get(3)?,
        title: r.get(4)?,
        author_handle: r.get(5)?,
        author_name: r.get(6)?,
        duration_sec: r.get(7)?,
        published_at: r.get(8)?,
        view_count: r.get(9)?,
        like_count: r.get(10)?,
        comment_count: r.get(11)?,
        description: r.get(12)?,
        fetched_at: r.get(13)?,
    })
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 会话存储（第二版）
// ---------------------------------------------------------------------------

impl SqliteStore {
    pub fn create_session(&mut self, title: Option<&str>) -> Result<String> {
        let id = new_id();
        self.conn.execute(
            "INSERT INTO sessions (id, created_at, title) VALUES (?1,?2,?3)",
            params![id, now_ts(), title],
        )?;
        Ok(id)
    }

    /// `--continue` 用：找**最近有活动**的会话，不是最近创建的。
    ///
    /// 昨天建的会话五分钟前还在聊、今天建的一小时前就丢了——该续昨天那个。
    /// 所以排序键是 `MAX(turns.created_at)`，没有 turn 时退回 session 自己的时间。
    pub fn latest_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.created_at,
                    COALESCE(MAX(t.created_at), s.created_at) AS activity
             FROM sessions s LEFT JOIN turns t ON t.session_id = s.id
             GROUP BY s.id ORDER BY activity DESC, s.created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        Ok(match rows.next()? {
            Some(r) => Some(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
            }),
            None => None,
        })
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.created_at,
                    COALESCE(MAX(t.created_at), s.created_at) AS activity
             FROM sessions s LEFT JOIN turns t ON t.session_id = s.id
             GROUP BY s.id ORDER BY activity DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_session(&mut self, session_id: &str) -> Result<()> {
        // turns / items 靠 ON DELETE CASCADE 跟着走（init 里开了 foreign_keys）
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        Ok(())
    }

    /// 一次提问的全部产物，**一个事务写完**。
    ///
    /// 按设计只在终态（Done / Failed）调用一次：循环中途只改内存。
    /// 半截历史落库的话，下次 `--continue` 会发出违反配对不变量的请求。
    pub fn save_turn(
        &mut self,
        session_id: &str,
        model: &str,
        status: TurnStatus,
        items: &[Item],
    ) -> Result<String> {
        let turn_id = new_id();
        let ts = now_ts();
        let tx = self.conn.transaction()?;

        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO turns (id, session_id, seq, model, status, created_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![turn_id, session_id, seq, model, status.as_str(), ts],
        )?;
        for it in items {
            tx.execute(
                "INSERT INTO items
                 (id, turn_id, idx, item_type, iteration, call_id, payload_json, raw_json, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    new_id(),
                    turn_id,
                    it.idx,
                    it.kind.as_str(),
                    it.iteration,
                    it.call_id,
                    it.payload.to_string(),
                    it.raw_json,
                    ts
                ],
            )?;
        }
        tx.commit()?;
        Ok(turn_id)
    }

    /// 重建对话历史。跨 turn 按 `(turns.seq, items.idx)` 接起来。
    ///
    /// **刻意不取 `raw_json`**：这个查询每轮循环都要跑一次，
    /// 而单条 SC 原始响应能有 2MB，带上就是每轮白读一遍再扔掉。
    pub fn load_history(&self, session_id: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.idx, i.item_type, i.iteration, i.call_id, i.payload_json
             FROM items i JOIN turns t ON t.id = i.turn_id
             WHERE t.session_id = ?1
             ORDER BY t.seq, i.idx",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let kind_str: String = r.get(1)?;
            let payload_str: String = r.get(4)?;
            Ok(Item {
                idx: r.get(0)?,
                // 不认识的类型当普通 assistant 消息处理，不让一条脏数据毁掉整个会话
                kind: ItemKind::from_db(&kind_str).unwrap_or(ItemKind::AssistantMessage),
                iteration: r.get(2)?,
                call_id: r.get(3)?,
                payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
                raw_json: None,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 单独取某次工具调用的原始响应。调试和补解析用，永不进上下文。
    pub fn get_raw_json(&self, session_id: &str, call_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.raw_json FROM items i JOIN turns t ON t.id = i.turn_id
             WHERE t.session_id = ?1 AND i.call_id = ?2
               AND i.item_type = 'function_call_output'
             ORDER BY t.seq DESC, i.idx DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id, call_id])?;
        Ok(match rows.next()? {
            Some(r) => r.get(0)?,
            None => None,
        })
    }

    pub fn list_turns(&self, session_id: &str) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, seq, model, status, created_at FROM turns
             WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let st: String = r.get(3)?;
            Ok(Turn {
                id: r.get(0)?,
                seq: r.get(1)?,
                model: r.get(2)?,
                status: if st == "done" {
                    TurnStatus::Done
                } else {
                    TurnStatus::Failed(st)
                },
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 自查配对不变量：有 function_call 却没有对应 output 的 call_id。
    ///
    /// 正常情况下永远是空的。非空说明某次请求发出去必然 400——
    /// 这是 items 单表（而不是消息、工具分两张表）最直接的好处。
    pub fn unpaired_call_ids(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.call_id FROM items i JOIN turns t ON t.id = i.turn_id
             WHERE t.session_id = ?1 AND i.item_type = 'function_call'
             AND NOT EXISTS (
               SELECT 1 FROM items o JOIN turns t2 ON t2.id = o.turn_id
               WHERE t2.session_id = ?1 AND o.item_type = 'function_call_output'
                 AND o.call_id = i.call_id
             )
             ORDER BY t.seq, i.idx",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> SqliteStore {
        SqliteStore::in_memory().unwrap()
    }

    // -----------------------------------------------------------------
    // 会话存储（第二版）
    // -----------------------------------------------------------------

    fn user_item(idx: i64, text: &str) -> Item {
        Item::user_message(idx, text)
    }

    fn call_item(idx: i64, iter: i64, call_id: &str, name: &str) -> Item {
        Item::function_call(
            idx,
            iter,
            call_id,
            name,
            &serde_json::json!({"query": "科普"}),
        )
    }

    fn out_item(idx: i64, iter: i64, call_id: &str, content: &str, raw: &str) -> Item {
        Item::function_call_output(idx, iter, call_id, content, false, Some(raw.to_string()))
    }

    #[test]
    fn a_turn_and_its_items_survive_a_round_trip() {
        let mut st = mem();
        let sid = st.create_session(Some("找科普博主")).unwrap();
        let items = vec![
            user_item(1, "帮我找几个做科普的博主"),
            Item::assistant_message(2, 1, "我先搜一下"),
            call_item(3, 1, "call_00_A", "search_videos"),
            out_item(4, 1, "call_00_A", "[结果：20 条]...", r#"{"videos":[]}"#),
            Item::assistant_message(5, 2, "推荐毕导THU"),
        ];
        st.save_turn(&sid, "deepseek-chat", TurnStatus::Done, &items)
            .unwrap();

        let back = st.load_history(&sid).unwrap();
        assert_eq!(back.len(), 5);
        assert_eq!(back[0].kind, ItemKind::UserMessage);
        assert_eq!(back[2].kind, ItemKind::FunctionCall);
        assert_eq!(back[2].call_id.as_deref(), Some("call_00_A"));
        assert_eq!(back[2].iteration, Some(1));
        assert_eq!(back[4].payload["text"], "推荐毕导THU");
    }

    #[test]
    fn load_history_does_not_drag_the_raw_json_along() {
        // 重建历史每轮都要跑一次。2MB 的原始响应混进来就是每轮白读一遍。
        let mut st = mem();
        let sid = st.create_session(None).unwrap();
        let huge = format!(r#"{{"junk":"{}"}}"#, "x".repeat(50_000));
        st.save_turn(
            &sid,
            "m",
            TurnStatus::Done,
            &[out_item(1, 1, "c1", "精简后的文本", &huge)],
        )
        .unwrap();

        let back = st.load_history(&sid).unwrap();
        assert_eq!(back[0].payload["content"], "精简后的文本");
        assert!(back[0].raw_json.is_none(), "重建历史时不该带 raw_json");

        // 但要能单独取出来 —— 这是它存在的全部理由
        let raw = st.get_raw_json(&sid, "c1").unwrap();
        assert!(raw.unwrap().len() > 50_000);
    }

    #[test]
    fn turns_are_numbered_and_history_spans_them_in_order() {
        let mut st = mem();
        let sid = st.create_session(None).unwrap();
        st.save_turn(&sid, "m", TurnStatus::Done, &[user_item(1, "第一次提问")])
            .unwrap();
        st.save_turn(&sid, "m", TurnStatus::Done, &[user_item(1, "第二次追问")])
            .unwrap();

        // 两个 turn 的 idx 都是 1，但历史必须按 turn 顺序接起来
        let back = st.load_history(&sid).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].payload["text"], "第一次提问");
        assert_eq!(back[1].payload["text"], "第二次追问");
    }

    #[test]
    fn a_failed_turn_is_recorded_as_failed_not_silently_dropped() {
        // 失败的 turn 和成功的长得不一样，不标就分不出
        let mut st = mem();
        let sid = st.create_session(None).unwrap();
        st.save_turn(
            &sid,
            "m",
            TurnStatus::Failed("超过迭代上限".into()),
            &[user_item(1, "问题")],
        )
        .unwrap();

        let turns = st.list_turns(&sid).unwrap();
        assert_eq!(turns.len(), 1);
        assert!(matches!(turns[0].status, TurnStatus::Failed(_)));
    }

    #[test]
    fn unpaired_tool_calls_can_be_found_with_one_query() {
        // 配对不变量的自查：有 function_call 没有对应的 output，
        // 那次请求必然 400。这是 items 单表最直接的好处。
        let mut st = mem();
        let sid = st.create_session(None).unwrap();
        st.save_turn(
            &sid,
            "m",
            TurnStatus::Failed("崩了".into()),
            &[
                call_item(1, 1, "call_paired", "search_videos"),
                out_item(2, 1, "call_paired", "ok", "{}"),
                call_item(3, 1, "call_orphan", "get_creator"), // 没有对应的 output
            ],
        )
        .unwrap();

        assert_eq!(st.unpaired_call_ids(&sid).unwrap(), vec!["call_orphan"]);
    }

    #[test]
    fn latest_session_picks_the_one_with_the_newest_activity() {
        // --continue 要续「最近活动的」，不是「最近创建的」。
        // 昨天建的会话五分钟前还在聊，今天建的一小时前就丢了 —— 该续昨天那个。
        let mut st = mem();
        let old = st.create_session(Some("老会话")).unwrap();
        let new = st.create_session(Some("新会话")).unwrap();
        // 老会话后发生了活动
        st.save_turn(&old, "m", TurnStatus::Done, &[user_item(1, "后来又聊了")])
            .unwrap();

        assert_eq!(
            st.latest_session().unwrap().unwrap().id,
            old,
            "该续有活动的那个"
        );
        assert_ne!(new, old);
    }

    #[test]
    fn deleting_a_session_takes_its_turns_and_items_with_it() {
        let mut st = mem();
        let sid = st.create_session(None).unwrap();
        st.save_turn(&sid, "m", TurnStatus::Done, &[user_item(1, "x")])
            .unwrap();
        st.delete_session(&sid).unwrap();

        assert!(st.load_history(&sid).unwrap().is_empty());
        assert!(st.list_turns(&sid).unwrap().is_empty());
    }

    use crate::content::model::{Artifact, new_id, now_ts};

    /// 三个端点都成功的 artifacts。
    fn all_ok() -> Vec<Artifact> {
        vec![
            Artifact::ok(ArtifactKind::Detail, r#"{"detail":true}"#.into()),
            Artifact::ok(ArtifactKind::Transcript, r#"{"transcript":true}"#.into()),
            Artifact::ok(ArtifactKind::Comments, r#"{"comments":true}"#.into()),
        ]
    }

    fn sample(native_id: &str, title: &str) -> FetchedVideo {
        let vid = new_id();
        FetchedVideo {
            video: Video {
                id: vid.clone(),
                platform: Platform::YouTube,
                native_id: native_id.to_string(),
                url: format!("https://youtube.com/watch?v={native_id}"),
                title: Some(title.to_string()),
                author_handle: Some("someone".into()),
                author_name: Some("Some One".into()),
                duration_sec: Some(57),
                published_at: Some(1772402418),
                view_count: Some(231),
                like_count: Some(1),
                comment_count: None,
                description: Some("desc".into()),
                fetched_at: now_ts(),
            },
            transcript: Some(Transcript {
                video_id: vid.clone(),
                text: "hello world".into(),
                source: "sc".into(),
                lang: Some("English".into()),
                fetched_at: now_ts(),
            }),
            comments: vec![
                Comment {
                    id: "c1".into(),
                    video_id: vid.clone(),
                    author: Some("a".into()),
                    text: "冷门评论".into(),
                    like_count: Some(3),
                    published_at: Some(1755412863),
                },
                Comment {
                    id: "c2".into(),
                    video_id: vid.clone(),
                    author: Some("b".into()),
                    text: "热门评论".into(),
                    like_count: Some(999),
                    published_at: Some(1755412863),
                },
            ],
            artifacts: all_ok(),
        }
    }

    #[test]
    fn saves_and_reads_back_everything() {
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "标题")).unwrap();

        let got = s
            .find_by_native(Platform::YouTube, "abc")
            .unwrap()
            .expect("应该查到");
        assert_eq!(got.video.title.as_deref(), Some("标题"));
        assert_eq!(got.video.duration_sec, Some(57));
        assert_eq!(got.video.comment_count, None, "NULL 应该原样读回来");
        assert_eq!(got.transcript.unwrap().text, "hello world");
        assert_eq!(got.comments.len(), 2);
    }

    #[test]
    fn comments_come_back_hottest_first() {
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "标题")).unwrap();
        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert_eq!(got.comments[0].text, "热门评论", "点赞多的排前面");
    }

    #[test]
    fn missing_video_returns_none_not_error() {
        let s = SqliteStore::in_memory().unwrap();
        assert!(
            s.find_by_native(Platform::YouTube, "nope")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resaving_same_video_updates_in_place_keeping_id() {
        let mut s = SqliteStore::in_memory().unwrap();
        let id1 = s.save(&sample("abc", "旧标题")).unwrap();
        let id2 = s.save(&sample("abc", "新标题")).unwrap();

        assert_eq!(id1, id2, "重抓同一个视频必须复用原来的 id");
        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert_eq!(got.video.title.as_deref(), Some("新标题"));
        assert_eq!(s.list_videos(100).unwrap().len(), 1, "不能变成两条记录");
    }

    #[test]
    fn resaving_replaces_comments_rather_than_accumulating() {
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "t")).unwrap();
        let mut second = sample("abc", "t");
        second.comments.truncate(1);
        s.save(&second).unwrap();

        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert_eq!(
            got.comments.len(),
            1,
            "重抓后评论应以最新一批为准，不能累积"
        );
    }

    #[test]
    fn video_without_transcript_is_fine() {
        // 纯画面 + BGM 的视频会走到这里
        let mut s = SqliteStore::in_memory().unwrap();
        let mut f = sample("silent", "无人声");
        f.transcript = None;
        f.comments.clear();
        s.save(&f).unwrap();

        let got = s
            .find_by_native(Platform::YouTube, "silent")
            .unwrap()
            .unwrap();
        assert!(got.transcript.is_none());
        assert!(got.comments.is_empty());
    }

    // -----------------------------------------------------------------------
    // 抓取失败不能破坏已有数据（Codex review 的 P1）
    // -----------------------------------------------------------------------

    #[test]
    fn failed_transcript_fetch_keeps_the_old_transcript() {
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "t")).unwrap();

        // 第二次抓：转录端点挂了
        let mut second = sample("abc", "t");
        second.transcript = None;
        second.artifacts = vec![
            Artifact::ok(ArtifactKind::Detail, "{}".into()),
            Artifact::failed(ArtifactKind::Transcript, "网络超时".into()),
            Artifact::ok(ArtifactKind::Comments, "{}".into()),
        ];
        s.save(&second).unwrap();

        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert_eq!(
            got.transcript.map(|t| t.text).as_deref(),
            Some("hello world"),
            "抓取失败时必须保留上次的转录，不能因为一次网络抖动就丢数据"
        );
    }

    #[test]
    fn failed_comments_fetch_keeps_the_old_comments() {
        // 这是原来最隐蔽的一个 bug：评论无条件先删后插，
        // 请求一失败就等于把旧评论清空了
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "t")).unwrap();

        let mut second = sample("abc", "t");
        second.comments.clear();
        second.artifacts = vec![
            Artifact::ok(ArtifactKind::Detail, "{}".into()),
            Artifact::ok(ArtifactKind::Transcript, "{}".into()),
            Artifact::failed(ArtifactKind::Comments, "HTTP 500".into()),
        ];
        s.save(&second).unwrap();

        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert_eq!(got.comments.len(), 2, "评论抓取失败时必须保留旧评论");
    }

    #[test]
    fn unavailable_does_clear_old_data() {
        // 和上面相反：调用成功、确认「就是没有」，这时该清掉旧数据。
        // 否则视频作者删了字幕，我们还一直拿着过期的转录。
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "t")).unwrap();

        let mut second = sample("abc", "t");
        second.transcript = None;
        second.comments.clear();
        second.artifacts = vec![
            Artifact::ok(ArtifactKind::Detail, "{}".into()),
            Artifact::unavailable(ArtifactKind::Transcript, r#"{"transcript":null}"#.into()),
            Artifact::unavailable(ArtifactKind::Comments, r#"{"comments":[]}"#.into()),
        ];
        s.save(&second).unwrap();

        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert!(got.transcript.is_none(), "确认没有时应该清掉旧转录");
        assert!(got.comments.is_empty(), "确认没有时应该清掉旧评论");
    }

    #[test]
    fn missing_artifact_record_is_treated_as_failed() {
        // 保守默认：没有状态记录时当失败处理，宁可保留旧数据也不误删
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "t")).unwrap();

        let mut second = sample("abc", "t");
        second.transcript = None;
        second.comments.clear();
        second.artifacts = vec![Artifact::ok(ArtifactKind::Detail, "{}".into())]; // 只有详情
        s.save(&second).unwrap();

        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().unwrap();
        assert!(got.transcript.is_some(), "没有状态记录时应保守保留旧数据");
        assert_eq!(got.comments.len(), 2);
    }

    #[test]
    fn stores_raw_response_of_every_endpoint() {
        // 之前只存了详情那一份原始响应，文字稿和评论的直接丢了——
        // 所以「解析漏字段随时能补」这个承诺并不成立。
        let mut s = SqliteStore::in_memory().unwrap();
        let id = s.save(&sample("abc", "t")).unwrap();

        let mut stmt = s
            .conn
            .prepare(
                "SELECT kind, status, raw_json FROM artifacts WHERE video_id = ?1 ORDER BY kind",
            )
            .unwrap();
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rows.len(), 3, "三个端点都要有记录");
        for (kind, status, raw) in &rows {
            assert_eq!(status, "ok");
            assert!(raw.is_some(), "{kind} 的原始响应必须存下来");
        }
        assert_eq!(rows[0].0, "comments");
        assert_eq!(rows[1].0, "detail");
        assert_eq!(rows[2].0, "transcript");
    }

    #[test]
    fn failed_artifact_records_the_error() {
        let mut s = SqliteStore::in_memory().unwrap();
        let mut f = sample("abc", "t");
        f.artifacts = vec![
            Artifact::ok(ArtifactKind::Detail, "{}".into()),
            Artifact::failed(ArtifactKind::Transcript, "AI 转录重试后仍失败".into()),
            Artifact::ok(ArtifactKind::Comments, "{}".into()),
        ];
        let id = s.save(&f).unwrap();

        let (status, err): (String, Option<String>) = s
            .conn
            .query_row(
                "SELECT status, error FROM artifacts WHERE video_id = ?1 AND kind = 'transcript'",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(err.as_deref(), Some("AI 转录重试后仍失败"));
    }

    #[test]
    fn migrates_legacy_raw_json_into_artifacts_without_losing_data() {
        // 手工搭一个旧版 schema：videos 带 raw_json 列，没有 artifacts 表
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE videos (
               id TEXT PRIMARY KEY, platform TEXT NOT NULL, native_id TEXT NOT NULL,
               url TEXT NOT NULL, title TEXT, author_handle TEXT, author_name TEXT,
               duration_sec INTEGER, published_at INTEGER, view_count INTEGER,
               like_count INTEGER, comment_count INTEGER, description TEXT,
               raw_json TEXT NOT NULL, fetched_at INTEGER NOT NULL,
               UNIQUE(platform, native_id));
             INSERT INTO videos (id, platform, native_id, url, raw_json, fetched_at)
             VALUES ('v-old', 'youtube', 'legacy', 'https://x', '{\"legacy\":true}', 111);",
        )
        .unwrap();

        // 打开时应自动迁移
        let store = SqliteStore::init(conn).unwrap();

        // 1) 老的原始响应被搬进了 artifacts，没丢
        let arts = store.get_artifacts("v-old").unwrap();
        assert_eq!(
            arts.len(),
            1,
            "旧的 raw_json 应该被搬成一条 detail artifact"
        );
        assert_eq!(arts[0].kind, ArtifactKind::Detail);
        assert_eq!(arts[0].status, FetchStatus::Ok);
        assert_eq!(arts[0].raw_json.as_deref(), Some(r#"{"legacy":true}"#));
        assert_eq!(arts[0].fetched_at, 111, "抓取时间要跟着一起搬");

        // 2) videos 表上的冗余列已经删掉
        let still_there = store
            .conn
            .prepare("PRAGMA table_info(videos)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|c| c == "raw_json");
        assert!(!still_there, "raw_json 列应该已经被删掉");

        // 3) 视频本身还在，能正常读出来
        assert!(
            store
                .find_by_native(Platform::YouTube, "legacy")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn migration_is_idempotent_on_fresh_db() {
        // 新库没有 raw_json 列，迁移应该直接跳过而不是报错
        let s = SqliteStore::in_memory().unwrap();
        assert!(migrate_drop_video_raw_json(&s.conn).is_ok());
        assert!(
            migrate_drop_video_raw_json(&s.conn).is_ok(),
            "跑两次也该没事"
        );
    }

    #[test]
    fn migration_does_not_overwrite_existing_detail_artifact() {
        // 已经有新版 detail artifact 的视频，迁移不该拿旧 raw_json 覆盖它
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE videos (
               id TEXT PRIMARY KEY, platform TEXT NOT NULL, native_id TEXT NOT NULL,
               url TEXT NOT NULL, title TEXT, author_handle TEXT, author_name TEXT,
               duration_sec INTEGER, published_at INTEGER, view_count INTEGER,
               like_count INTEGER, comment_count INTEGER, description TEXT,
               raw_json TEXT NOT NULL, fetched_at INTEGER NOT NULL,
               UNIQUE(platform, native_id));
             CREATE TABLE artifacts (
               video_id TEXT NOT NULL, kind TEXT NOT NULL, status TEXT NOT NULL,
               raw_json TEXT, error TEXT, fetched_at INTEGER NOT NULL,
               PRIMARY KEY (video_id, kind));
             INSERT INTO videos (id, platform, native_id, url, raw_json, fetched_at)
             VALUES ('v1', 'youtube', 'a', 'https://x', '{\"old\":true}', 100);
             INSERT INTO artifacts VALUES ('v1', 'detail', 'ok', '{\"new\":true}', NULL, 200);",
        )
        .unwrap();

        let store = SqliteStore::init(conn).unwrap();
        let arts = store.get_artifacts("v1").unwrap();
        assert_eq!(
            arts[0].raw_json.as_deref(),
            Some(r#"{"new":true}"#),
            "新数据不该被旧的覆盖"
        );
    }

    #[test]
    fn list_videos_is_newest_first() {
        let mut s = SqliteStore::in_memory().unwrap();
        let mut old = sample("old", "旧的");
        old.video.fetched_at = 1000;
        let mut new = sample("new", "新的");
        new.video.fetched_at = 2000;
        s.save(&old).unwrap();
        s.save(&new).unwrap();

        let list = s.list_videos(10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].title.as_deref(), Some("新的"), "最近抓的排前面");
    }
}

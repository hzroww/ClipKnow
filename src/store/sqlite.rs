//! SQLite 实现。
//!
//! rusqlite 是 SQLite 这个 C 库的 Rust 封装，相当于 C++ 里的 SQLiteCpp。
//! `bundled` feature 让它把 SQLite 源码一起编译进来，所以你的机器上
//! 不需要另外装任何东西。

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::content::model::{ArtifactKind, Comment, FetchedVideo, Transcript, Video};
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
        Ok(Self { conn })
    }
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

        Ok(Some(StoredVideo { video, transcript, comments }))
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
                                 description, raw_json, fetched_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(platform, native_id) DO UPDATE SET
                url=excluded.url, title=excluded.title,
                author_handle=excluded.author_handle, author_name=excluded.author_name,
                duration_sec=excluded.duration_sec, published_at=excluded.published_at,
                view_count=excluded.view_count, like_count=excluded.like_count,
                comment_count=excluded.comment_count, description=excluded.description,
                raw_json=excluded.raw_json, fetched_at=excluded.fetched_at",
            params![
                video_id, v.platform.as_str(), v.native_id, v.url, v.title,
                v.author_handle, v.author_name, v.duration_sec, v.published_at,
                v.view_count, v.like_count, v.comment_count, v.description,
                v.raw_json, v.fetched_at
            ],
        )?;

        // ★ 文字稿：只有「结论明确」时才动旧数据。
        // 抓取失败（网络错误 / AI 转录挂了）时保持原样——否则 --refresh
        // 碰上一次抖动，就把上次抓好的内容弄丢了。
        if fetched.status_of(ArtifactKind::Transcript).is_conclusive() {
            tx.execute("DELETE FROM transcripts WHERE video_id = ?1", params![video_id])?;
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
            tx.execute("DELETE FROM comments WHERE video_id = ?1", params![video_id])?;
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

    fn list_videos(&self, limit: usize) -> Result<Vec<Video>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {VIDEO_COLS} FROM videos ORDER BY fetched_at DESC LIMIT ?1"))?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_video)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// 列名列一次，SELECT 的地方复用，避免顺序和 `row_to_video` 对不上。
const VIDEO_COLS: &str = "id, platform, native_id, url, title, author_handle, author_name, \
     duration_sec, published_at, view_count, like_count, comment_count, description, \
     raw_json, fetched_at";

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
        raw_json: r.get(13)?,
        fetched_at: r.get(14)?,
    })
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::{new_id, now_ts, Artifact};

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
                raw_json: r#"{"ok":true}"#.into(),
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
                    id: "c1".into(), video_id: vid.clone(), author: Some("a".into()),
                    text: "冷门评论".into(), like_count: Some(3), published_at: Some(1755412863),
                },
                Comment {
                    id: "c2".into(), video_id: vid.clone(), author: Some("b".into()),
                    text: "热门评论".into(), like_count: Some(999), published_at: Some(1755412863),
                },
            ],
            artifacts: all_ok(),
        }
    }

    #[test]
    fn saves_and_reads_back_everything() {
        let mut s = SqliteStore::in_memory().unwrap();
        s.save(&sample("abc", "标题")).unwrap();

        let got = s.find_by_native(Platform::YouTube, "abc").unwrap().expect("应该查到");
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
        assert!(s.find_by_native(Platform::YouTube, "nope").unwrap().is_none());
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
        assert_eq!(got.comments.len(), 1, "重抓后评论应以最新一批为准，不能累积");
    }

    #[test]
    fn video_without_transcript_is_fine() {
        // 纯画面 + BGM 的视频会走到这里
        let mut s = SqliteStore::in_memory().unwrap();
        let mut f = sample("silent", "无人声");
        f.transcript = None;
        f.comments.clear();
        s.save(&f).unwrap();

        let got = s.find_by_native(Platform::YouTube, "silent").unwrap().unwrap();
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
            .prepare("SELECT kind, status, raw_json FROM artifacts WHERE video_id = ?1 ORDER BY kind")
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

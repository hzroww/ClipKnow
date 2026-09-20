//! 一次性的会话认领：把邀请码时代的归属关系搬进 `sessions.user_id`。
//!
//! ## 为什么这一半在 Rust 这边
//!
//! 账号表是 Go 的，`sessions` 是 Rust 的。导入分两步：
//!
//!   Go（`-import-accounts`）  建 users，把 user_id 写回 access.json
//!   Rust（这里）              读 access.json，填 sessions.user_id
//!
//! 一个命令干完当然更省事，但那样 Go 就得写 Rust 的表。这是一次性的离线工具，
//! 破例一次没人会发现——然后下一个人就照着破例了。边界是靠不破例维持的。
//!
//! ## 没有归属的那些会话
//!
//! 命令行（`find` / `ask`）建的会话从来没进过 access.json 的 owner 映射，所以
//! 认不出主人。设计文档要求「由操作者明确指定归属或隔离」，不能猜——这里做成
//! 必须显式给 `--unowned`，没给就只报数不动手。

use crate::error::{ClipKnowError, Result};
use rusqlite::Connection;
use std::collections::HashMap;

/// access.json 里我们需要的那两块。
#[derive(serde::Deserialize)]
pub struct AccessFile {
    users: HashMap<String, AccessUser>,
    owner: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
struct AccessUser {
    #[serde(default)]
    user_id: String,
}

pub struct ClaimReport {
    pub claimed: usize,
    pub unowned: Vec<String>,
    pub already: usize,
    pub missing_user: Vec<String>,
}

impl AccessFile {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ClipKnowError::BadRequest(format!("读不了 {path}: {e}")))?;
        serde_json::from_str(&raw)
            .map_err(|e| ClipKnowError::BadRequest(format!("{path} 不是合法 JSON: {e}")))
    }

    /// 码 → user_id。没导入过（user_id 为空）的码不进这张表。
    fn code_to_user(&self) -> HashMap<&str, &str> {
        self.users
            .iter()
            .filter(|(_, u)| !u.user_id.is_empty())
            .map(|(c, u)| (c.as_str(), u.user_id.as_str()))
            .collect()
    }
}

/// 把归属写进库。`unowned` 给了就把无主会话也归给它。
///
/// 整批在一个事务里：要么全填上，要么一条不动。中途失败留下「一半有主一半
/// 没主」的库，比完全没跑更难收拾。
pub fn claim(
    conn: &mut Connection,
    access: &AccessFile,
    unowned: Option<&str>,
) -> Result<ClaimReport> {
    let map = access.code_to_user();
    let mut rep = ClaimReport {
        claimed: 0,
        unowned: Vec::new(),
        already: 0,
        missing_user: Vec::new(),
    };

    let ids: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, user_id FROM sessions")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    let tx = conn.transaction()?;
    for (sid, existing) in &ids {
        if existing.is_some() {
            rep.already += 1;
            continue;
        }
        let target = match access.owner.get(sid) {
            Some(code) => match map.get(code.as_str()) {
                Some(uid) => Some((*uid).to_string()),
                None => {
                    // 码在 owner 里，但那个码还没导入成账号
                    rep.missing_user.push(sid.clone());
                    continue;
                }
            },
            None => unowned.map(str::to_string),
        };
        match target {
            Some(uid) => {
                tx.execute(
                    "UPDATE sessions SET user_id = ?1 WHERE id = ?2 AND user_id IS NULL",
                    rusqlite::params![uid, sid],
                )?;
                rep.claimed += 1;
            }
            None => rep.unowned.push(sid.clone()),
        }
    }
    tx.commit()?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::store::migrate::run(&mut c).unwrap();
        for (id, name) in [("ua", "阿"), ("ub", "波")] {
            c.execute(
                "INSERT INTO users(id,username_normalized,display_name,password_hash,status,created_at,updated_at)
                 VALUES (?1,?1,?2,'h','active',0,0)",
                rusqlite::params![id, name],
            )
            .unwrap();
        }
        for sid in ["s1", "s2", "s3"] {
            c.execute(
                "INSERT INTO sessions(id,created_at,title) VALUES (?1,0,'t')",
                [sid],
            )
            .unwrap();
        }
        c
    }

    fn access(json: &str) -> AccessFile {
        serde_json::from_str(json).unwrap()
    }

    const TWO_OWNED: &str = r#"{
        "users": {"CODE_A": {"user_id": "ua"}, "CODE_B": {"user_id": "ub"}},
        "owner": {"s1": "CODE_A", "s2": "CODE_B"}
    }"#;

    fn owner_of(c: &Connection, sid: &str) -> Option<String> {
        c.query_row("SELECT user_id FROM sessions WHERE id=?1", [sid], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn 按_owner_映射归位() {
        let mut c = db();
        let rep = claim(&mut c, &access(TWO_OWNED), None).unwrap();
        assert_eq!(rep.claimed, 2);
        assert_eq!(owner_of(&c, "s1").as_deref(), Some("ua"));
        assert_eq!(owner_of(&c, "s2").as_deref(), Some("ub"));
    }

    #[test]
    fn 没有归属记录的不给_unowned_就一条不动() {
        // 命令行建的会话属于谁只有操作者知道，不能猜。
        let mut c = db();
        let rep = claim(&mut c, &access(TWO_OWNED), None).unwrap();
        assert_eq!(rep.unowned, vec!["s3"]);
        assert_eq!(owner_of(&c, "s3"), None, "没给 --unowned 就不该动它");
    }

    #[test]
    fn 给了_unowned_才归给那个人() {
        let mut c = db();
        let rep = claim(&mut c, &access(TWO_OWNED), Some("ua")).unwrap();
        assert_eq!(rep.claimed, 3);
        assert_eq!(owner_of(&c, "s3").as_deref(), Some("ua"));
    }

    #[test]
    fn 重复跑不会改已经有主的() {
        let mut c = db();
        claim(&mut c, &access(TWO_OWNED), None).unwrap();
        // 第二次即使给了 unowned，也不能把 s1 从 ua 抢给 ub
        let rep = claim(&mut c, &access(TWO_OWNED), Some("ub")).unwrap();
        assert_eq!(rep.already, 2);
        assert_eq!(owner_of(&c, "s1").as_deref(), Some("ua"), "不能改已有归属");
        assert_eq!(
            owner_of(&c, "s3").as_deref(),
            Some("ub"),
            "无主的才归给新人"
        );
    }

    #[test]
    fn 码还没导入成账号时跳过而不是乱归() {
        // user_id 为空 = 这个码还没跑过 -import-accounts。
        // 这时候把会话归给别人是错的，宁可跳过并报出来。
        let mut c = db();
        let a = access(r#"{"users": {"CODE_A": {"user_id": ""}}, "owner": {"s1": "CODE_A"}}"#);
        let rep = claim(&mut c, &a, None).unwrap();
        assert_eq!(rep.missing_user, vec!["s1"]);
        assert_eq!(rep.claimed, 0);
        assert_eq!(owner_of(&c, "s1"), None);
    }

    #[test]
    fn 归给不存在的用户会被外键挡住() {
        // sessions.user_id 有外键约束，写一个不存在的 id 必须失败，
        // 而不是留下一条指向空气的记录。
        let mut c = db();
        let err = claim(&mut c, &access(TWO_OWNED), Some("不存在的用户"));
        assert!(err.is_err(), "外键该拦住它");
        assert_eq!(owner_of(&c, "s1"), None, "整批回滚，一条都不该写进去");
    }
}

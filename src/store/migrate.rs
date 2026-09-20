//! 迁移入口。
//!
//! ## 为什么要有版本表
//!
//! 以前每次 `Store::open()` 都把全部 DDL 重跑一遍，靠 `CREATE TABLE IF NOT
//! EXISTS` 和 `PRAGMA table_info` 检查做到幂等。单进程时这样能用，但多用户之后
//! 有两个问题：
//!
//! 1. **Go 和 Rust 会同时启动**，两边都想跑 DDL 就是两个写者抢锁。
//! 2. 幂等靠的是每条迁移自己写对守卫。漏一个就在启动时炸，而且是在生产环境。
//!
//! 现在改成：版本表记已经应用到第几号，`clipknow migrate` 是唯一执行入口，
//! 服务启动**只检查版本**，落后就报错并告诉你跑什么命令。
//!
//! ## 为什么 baseline 是一整块
//!
//! 001–006 那六个文件加三个 Rust 数据迁移，历史上是当作一个序列跑的，而且整体
//! 幂等。把它们重新切成六个版本号，对已经存在的库要逐个判断「这条到底跑过没
//! 有」——判断错了就是线上事故。所以它们合成版本 1「baseline」，新的从 2 开始。
//!
//! 已有迁移不回改，只往后加。

use crate::error::Result;
use rusqlite::Connection;

/// 一条迁移。SQL 的用 `Sql`，需要读写数据或查 PRAGMA 的用 `Code`。
enum Step {
    Sql(&'static str),
    Code(fn(&Connection) -> Result<()>),
}

struct Migration {
    version: i64,
    name: &'static str,
    steps: &'static [Step],
}

/// ★ 只往后加，不改已有的。改了的话，已经升级过的库和新库会长得不一样。
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "baseline",
        steps: &[
            Step::Sql(include_str!("../../migrations/001_init.sql")),
            Step::Code(super::sqlite::migrate_drop_video_raw_json),
            Step::Sql(include_str!("../../migrations/002_agent_loop.sql")),
            Step::Code(super::sqlite::migrate_add_turn_summary),
            Step::Sql(include_str!("../../migrations/004_video_dossier.sql")),
            Step::Code(super::sqlite::migrate_add_dossier_staging),
            Step::Code(super::sqlite::migrate_add_dossier_failure),
        ],
    },
    Migration {
        version: 2,
        name: "multi_user",
        // ★ 一条迁移可以带多个拥有方的 SQL。文件按拥有方分目录放，执行仍由
        //   这一个 runner 按全局版本号顺序跑——「单一 runner + 全局版本号」
        //   和「文件按业务分开」不矛盾。
        steps: &[
            // Go 拥有
            Step::Sql(include_str!("../../migrations/accounts/007a_users.sql")),
            // Rust 拥有
            Step::Sql(include_str!(
                "../../migrations/agent/007_session_ownership.sql"
            )),
        ],
    },
];

/// 代码期望的库版本。
pub fn expected_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

fn ensure_version_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             name       TEXT NOT NULL,
             applied_at INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

/// 库现在到第几号。没有版本表就是 0。
pub fn current_version(conn: &Connection) -> Result<i64> {
    let has: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has {
        return Ok(0);
    }
    let v: Option<i64> = conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
        r.get(0)
    })?;
    Ok(v.unwrap_or(0))
}

/// 这个库是不是「升级之前就已经在用」的老库。
///
/// 判断依据：有 sessions 表但没有版本表。老库的 schema 已经是 baseline 的样子
/// 了，重跑一遍虽然幂等、也没坏处，但白白持有一次写锁；直接记成已应用更干净。
fn is_legacy(conn: &Connection) -> Result<bool> {
    let has_sessions: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    Ok(has_sessions && current_version(conn)? == 0)
}

fn mark_applied(conn: &Connection, m: &Migration) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![m.version, m.name, crate::content::model::now_ts()],
    )?;
    Ok(())
}

/// 把还没应用的迁移跑掉。**唯一执行 DDL 的地方。**
///
/// 每条迁移连同它的版本记录在同一个事务里提交——中途崩了要么整条没跑、要么
/// 跑完并记上，不会出现「跑了一半但版本号没动」这种下次重跑会炸的状态。
pub fn run(conn: &mut Connection) -> Result<Vec<i64>> {
    ensure_version_table(conn)?;
    let legacy = is_legacy(conn)?;
    let from = current_version(conn)?;
    let mut applied = Vec::new();

    for m in MIGRATIONS {
        if m.version <= from {
            continue;
        }
        let tx = conn.transaction()?;
        // 老库的 baseline 只记账不执行
        if !(legacy && m.version == 1) {
            for step in m.steps {
                match step {
                    Step::Sql(sql) => tx.execute_batch(sql)?,
                    Step::Code(f) => f(&tx)?,
                }
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, name, applied_at) VALUES (?1,?2,?3)",
            rusqlite::params![m.version, m.name, crate::content::model::now_ts()],
        )?;
        tx.commit()?;
        applied.push(m.version);
    }
    let _ = mark_applied; // 留着给将来手工补记用
    Ok(applied)
}

/// 服务启动时调用：只检查，不执行。
pub fn check(conn: &Connection) -> Result<()> {
    let have = current_version(conn)?;
    let want = expected_version();
    if have == want {
        return Ok(());
    }
    if have < want {
        return Err(crate::error::ClipKnowError::BadRequest(format!(
            "数据库还停在第 {have} 版，代码要第 {want} 版。先跑一次：clipknow migrate"
        )));
    }
    Err(crate::error::ClipKnowError::BadRequest(format!(
        "数据库是第 {have} 版，比这个程序（第 {want} 版）还新。\
         多半是回退了代码却没回退库——用对应版本的程序，或者从备份恢复。"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        c
    }

    #[test]
    fn 全新库从零建到最新版() {
        let mut c = fresh();
        let applied = run(&mut c).unwrap();
        assert_eq!(applied, vec![1, 2], "两条迁移都该跑");
        assert_eq!(current_version(&c).unwrap(), expected_version());
        // 关键表都在
        for t in [
            "sessions",
            "turns",
            "items",
            "videos",
            "users",
            "auth_sessions",
        ] {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{t} 表没建出来");
        }
    }

    #[test]
    fn 重复跑不会再动任何东西() {
        let mut c = fresh();
        run(&mut c).unwrap();
        let second = run(&mut c).unwrap();
        assert!(second.is_empty(), "第二次不该再应用任何迁移");
    }

    #[test]
    fn 老库的_baseline_只记账不重跑() {
        // 造一个「已经有 sessions 表、但没有版本表」的老库
        let mut c = fresh();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at INTEGER, title TEXT);",
        )
        .unwrap();

        // baseline 若真的执行，001_init.sql 会因为 sessions 已存在而走
        // CREATE TABLE IF NOT EXISTS——不报错，但也建不出 turns。
        // 这里断言的是「它被跳过了」：版本记上了，而 turns 确实没有。
        let applied = run(&mut c).unwrap();
        assert_eq!(applied, vec![1, 2]);

        let baseline_ran: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='turns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(baseline_ran, 0, "老库的 baseline 应该只记账，不执行");

        // 但 007 那条必须真的跑了——老库也要拿到 users 表
        let users: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='users'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(users, 1, "multi_user 那条对老库必须真执行");
    }

    #[test]
    fn 版本落后时_check_报错并告诉你跑什么() {
        let c = fresh();
        let err = check(&c).unwrap_err().to_string();
        assert!(err.contains("clipknow migrate"), "报错要给出命令：{err}");
        assert!(err.contains("第 0 版"), "报错要说清差在哪：{err}");
    }

    #[test]
    fn 库比程序新时_check_也要拦住() {
        let mut c = fresh();
        run(&mut c).unwrap();
        c.execute(
            "INSERT INTO schema_migrations(version,name,applied_at) VALUES (?1,'future',0)",
            [expected_version() + 1],
        )
        .unwrap();
        let err = check(&c).unwrap_err().to_string();
        assert!(err.contains("比这个程序"), "要提示是回退了代码：{err}");
    }

    #[test]
    fn 用户名唯一约束真的生效() {
        // 防重复注册靠的是这个约束，不是「先查一下没有再插入」——
        // 那两步之间有竞态窗口。
        let mut c = fresh();
        run(&mut c).unwrap();
        let ins = "INSERT INTO users(id,username_normalized,display_name,password_hash,status,created_at,updated_at)
                   VALUES (?1,'alice','Alice','$argon2id$fake','active',0,0)";
        c.execute(ins, ["u1"]).unwrap();
        assert!(c.execute(ins, ["u2"]).is_err(), "同名第二次插入必须失败");
    }

    #[test]
    fn 会话创建幂等键在同一用户下唯一() {
        let mut c = fresh();
        run(&mut c).unwrap();
        c.execute(
            "INSERT INTO users(id,username_normalized,display_name,password_hash,status,created_at,updated_at)
             VALUES ('u1','alice','Alice','h','active',0,0)",
            [],
        )
        .unwrap();
        let ins = "INSERT INTO sessions(id,created_at,title,user_id,creation_request_id)
                   VALUES (?1,0,'t','u1','req-1')";
        c.execute(ins, ["s1"]).unwrap();
        assert!(
            c.execute(ins, ["s2"]).is_err(),
            "同一个创建请求只能建一个会话"
        );

        // creation_request_id 为 NULL 的老会话不受约束（部分索引的 WHERE 条件）
        let old = "INSERT INTO sessions(id,created_at,title,user_id) VALUES (?1,0,'t','u1')";
        c.execute(old, ["s3"]).unwrap();
        c.execute(old, ["s4"]).unwrap();
    }
}

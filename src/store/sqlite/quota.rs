use super::*;
use serde_json::{Value, json};

// Account roles are read-only here; Go remains the only writer of users.
// Read the current DB role rather than trusting browser-supplied flags.
fn unlimited(conn: &Connection, user: &str) -> Result<bool> {
    Ok(
        conn.query_row("SELECT is_admin FROM users WHERE id=?1", [user], |r| {
            r.get::<_, bool>(0)
        })?,
    )
}

pub(super) fn charge(tx: &rusqlite::Transaction<'_>, user: &str, ts: i64) -> Result<()> {
    if unlimited(tx, user)? {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO user_quotas(user_id,updated_at) VALUES (?1,?2)",
        params![user, ts],
    )?;
    let changed = tx.execute(
        "UPDATE user_quotas SET free_used=free_used+1,updated_at=?2 WHERE user_id=?1 AND free_used<free_limit",
        params![user,ts],
    )?;
    if changed == 0 {
        return Err(crate::error::ClipKnowError::FreeQuotaExhausted);
    }
    Ok(())
}

pub(super) fn ensure_available(conn: &Connection, user: &str) -> Result<()> {
    if unlimited(conn, user)? {
        return Ok(());
    }
    let exhausted = conn
        .query_row(
            "SELECT free_used>=free_limit FROM user_quotas WHERE user_id=?1",
            [user],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    if exhausted {
        return Err(crate::error::ClipKnowError::FreeQuotaExhausted);
    }
    Ok(())
}

impl SqliteStore {
    pub fn user_quota(&self, user: &str) -> Result<Value> {
        let unlimited = unlimited(&self.conn, user)?;
        let (limit, used) = self
            .conn
            .query_row(
                "SELECT free_limit,free_used FROM user_quotas WHERE user_id=?1",
                [user],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .unwrap_or((5, 0));
        Ok(
            json!({"limit":limit,"used":used,"remaining":if unlimited { None } else { Some(limit-used) },"unlimited":unlimited}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_user(st: &SqliteStore, user: &str, admin: bool) {
        st.conn.execute("INSERT INTO users(id,username_normalized,display_name,password_hash,created_at,updated_at,is_admin) VALUES (?1,?1,?1,'test',0,0,?2)",params![user,admin]).unwrap();
    }
    fn claim(st: &mut SqliteStore, user: &str, rid: &str) -> String {
        match st.claim_chat_request(user, rid, None, "question").unwrap() {
            ChatRequestClaim::Proceed { session_id } => session_id,
            _ => panic!("expected new request"),
        }
    }

    #[test]
    fn lifetime_limit_retries_failures_and_admin_role() {
        let mut st = SqliteStore::in_memory().unwrap();
        add_user(&st, "u", false);
        let old = st.create_session(None, Some("u")).unwrap();
        st.save_turn(
            &old,
            "test",
            TurnStatus::Done,
            &[Item::user_message(1, "old")],
        )
        .unwrap();
        assert_eq!(st.user_quota("u").unwrap()["remaining"], 5);
        // Reserved but not yet accepted requests must also recheck at begin_turn.
        let sixth_session = claim(&mut st, "u", "six");
        for i in 0..5 {
            let rid = format!("r{i}");
            let sid = claim(&mut st, "u", &rid);
            let tid = new_id();
            st.begin_turn(&tid, &sid, "test", "question", "title", Some(("u", &rid)))
                .unwrap();
            st.finish_turn(
                &tid,
                &sid,
                if i % 2 == 0 {
                    TurnStatus::Cancelled
                } else {
                    TurnStatus::Failed("failed".into())
                },
                &[],
            )
            .unwrap();
            assert!(matches!(
                st.claim_chat_request("u", &rid, None, "question").unwrap(),
                ChatRequestClaim::Duplicate { .. }
            ));
            assert!(
                st.begin_turn(
                    &new_id(),
                    &sid,
                    "test",
                    "question",
                    "title",
                    Some(("u", &rid))
                )
                .is_err()
            );
            assert_eq!(st.user_quota("u").unwrap()["used"], i + 1);
        }
        assert!(matches!(
            st.claim_chat_request("u", "new-six", None, "question"),
            Err(crate::error::ClipKnowError::FreeQuotaExhausted)
        ));
        let sid = sixth_session;
        assert!(matches!(
            st.begin_turn(
                &new_id(),
                &sid,
                "test",
                "question",
                "title",
                Some(("u", "six"))
            ),
            Err(crate::error::ClipKnowError::FreeQuotaExhausted)
        ));
        assert_eq!(
            st.conn
                .query_row(
                    "SELECT count(*) FROM turns WHERE session_id=?1",
                    [&sid],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        st.conn
            .execute("UPDATE users SET is_admin=1 WHERE id='u'", [])
            .unwrap();
        assert_eq!(st.user_quota("u").unwrap()["unlimited"], true);
        st.begin_turn(
            &new_id(),
            &sid,
            "test",
            "question",
            "title",
            Some(("u", "six")),
        )
        .unwrap();
        assert_eq!(st.user_quota("u").unwrap()["used"], 5);
        st.conn
            .execute("UPDATE users SET is_admin=0 WHERE id='u'", [])
            .unwrap();
        assert_eq!(st.user_quota("u").unwrap()["remaining"], 0);
        add_user(&st, "other", false);
        assert_eq!(st.user_quota("other").unwrap()["remaining"], 5);
    }

    #[test]
    fn failed_transaction_rolls_back_quota_and_request_binding() {
        let mut st = SqliteStore::in_memory().unwrap();
        add_user(&st, "u", false);
        let sid = claim(&mut st, "u", "r");
        st.conn.execute_batch("CREATE TRIGGER fail_touch BEFORE UPDATE OF updated_at ON sessions BEGIN SELECT RAISE(ABORT,'test failure after charge'); END;").unwrap();
        assert!(
            st.begin_turn(
                &new_id(),
                &sid,
                "test",
                "question",
                "title",
                Some(("u", "r"))
            )
            .is_err()
        );
        assert_eq!(st.user_quota("u").unwrap()["used"], 0);
        assert!(matches!(
            st.claim_chat_request("u", "r", None, "question").unwrap(),
            ChatRequestClaim::Proceed { .. }
        ));
        st.conn.execute_batch("DROP TRIGGER fail_touch;").unwrap();
        st.begin_turn(
            &new_id(),
            &sid,
            "test",
            "question",
            "title",
            Some(("u", "r")),
        )
        .unwrap();
        assert_eq!(st.user_quota("u").unwrap()["used"], 1);
    }

    #[test]
    fn simultaneous_connections_cannot_spend_more_than_five() {
        let db = crate::store::testdb::TempDb::new("quota");
        let path = db.path().to_string();
        let mut st = SqliteStore::open(&path).unwrap();
        add_user(&st, "u", false);
        let requests: Vec<_> = (0..12)
            .map(|i| {
                let rid = format!("r{i}");
                let sid = claim(&mut st, "u", &rid);
                (rid, sid)
            })
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(requests.len()));
        let workers: Vec<_> = requests
            .into_iter()
            .map(|(rid, sid)| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut st = SqliteStore::open(&path).unwrap();
                    barrier.wait();
                    match st.begin_turn(
                        &new_id(),
                        &sid,
                        "test",
                        "question",
                        "title",
                        Some(("u", &rid)),
                    ) {
                        Ok(_) => true,
                        Err(crate::error::ClipKnowError::FreeQuotaExhausted) => false,
                        Err(e) => panic!("unexpected failure: {e}"),
                    }
                })
            })
            .collect();
        let successes = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(successes, 5);
        assert_eq!(st.user_quota("u").unwrap()["used"], 5);
    }
}

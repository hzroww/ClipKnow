//! 管理后台只读查询。保留原始条目，不套用模型上下文的状态过滤和压缩逻辑。
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::SqliteStore;
use crate::error::Result;

fn page(mut rows: Vec<Value>, limit: usize, offset: usize, key: &str) -> Value {
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    json!({key: rows, "limit": limit, "offset": offset, "has_more": has_more})
}

impl SqliteStore {
    pub fn admin_sessions(&self, user_id: &str, limit: usize, offset: usize) -> Result<Value> {
        let mut stmt = self.conn.prepare(
            "SELECT id,title,created_at,updated_at,deleted_at FROM sessions
             WHERE user_id=?1 ORDER BY COALESCE(updated_at,created_at) DESC,id DESC
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![user_id, (limit + 1) as i64, offset as i64], |r| {
            Ok(
                json!({"id":r.get::<_,String>(0)?,"title":r.get::<_,Option<String>>(1)?,
                "created_at":r.get::<_,i64>(2)?,"updated_at":r.get::<_,Option<i64>>(3)?,
                "deleted_at":r.get::<_,Option<i64>>(4)?}),
            )
        })?;
        let mut out = page(
            rows.collect::<std::result::Result<Vec<_>, _>>()?,
            limit,
            offset,
            "sessions",
        );
        out["user_id"] = json!(user_id);
        Ok(out)
    }

    pub fn admin_turns(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Option<Value>> {
        let session = self
            .conn
            .query_row(
                "SELECT id,user_id,title,created_at FROM sessions WHERE id=?1",
                [session_id],
                |r| {
                    Ok(
                        json!({"id":r.get::<_,String>(0)?,"user_id":r.get::<_,Option<String>>(1)?,
                    "title":r.get::<_,Option<String>>(2)?,"created_at":r.get::<_,i64>(3)?}),
                    )
                },
            )
            .optional()?;
        let Some(session) = session else {
            return Ok(None);
        };
        // 列表只取短预览。完整正文和大的 raw_json 在展开时另取。
        let mut stmt = self.conn.prepare(
            "SELECT t.id,t.seq,t.model,t.status,t.created_at,
             (SELECT substr(json_extract(i.payload_json,'$.text'),1,240) FROM items i
              WHERE i.turn_id=t.id AND i.item_type='user_message' ORDER BY i.idx LIMIT 1),
             (SELECT substr(json_extract(i.payload_json,'$.text'),1,240) FROM items i
              WHERE i.turn_id=t.id AND i.item_type='assistant_message' ORDER BY i.idx DESC LIMIT 1)
             FROM turns t WHERE t.session_id=?1 ORDER BY t.seq,t.id LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(
            params![session_id, (limit + 1) as i64, offset as i64],
            |r| {
                Ok(json!({"id":r.get::<_,String>(0)?,"seq":r.get::<_,i64>(1)?,
                "model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,
                "created_at":r.get::<_,i64>(4)?,"question_preview":r.get::<_,Option<String>>(5)?,
                "answer_preview":r.get::<_,Option<String>>(6)?}))
            },
        )?;
        let mut out = page(
            rows.collect::<std::result::Result<Vec<_>, _>>()?,
            limit,
            offset,
            "turns",
        );
        out["session"] = session;
        Ok(Some(out))
    }

    pub fn admin_turn(&self, turn_id: &str) -> Result<Option<Value>> {
        let turn = self.conn.query_row(
            "SELECT t.id,t.session_id,s.user_id,t.seq,t.model,t.status,t.created_at,t.summary,t.summarized_upto
             FROM turns t JOIN sessions s ON s.id=t.session_id WHERE t.id=?1", [turn_id], |r| {
                Ok(json!({"id":r.get::<_,String>(0)?,"session_id":r.get::<_,String>(1)?,
                    "user_id":r.get::<_,Option<String>>(2)?,"seq":r.get::<_,i64>(3)?,
                    "model":r.get::<_,String>(4)?,"status":r.get::<_,String>(5)?,
                    "created_at":r.get::<_,i64>(6)?,"summary":r.get::<_,Option<String>>(7)?,
                    "summarized_upto":r.get::<_,Option<i64>>(8)?}))
            },
        ).optional()?;
        let Some(turn) = turn else { return Ok(None) };
        let mut stmt = self.conn.prepare(
            "SELECT id,idx,item_type,iteration,call_id,payload_json,raw_json IS NOT NULL
             FROM items WHERE turn_id=?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map([turn_id], |r| {
            let raw: String = r.get(5)?;
            // 老数据若不是合法 JSON 也保留原文，不能悄悄丢掉调试证据。
            let payload = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
            Ok(json!({"id":r.get::<_,String>(0)?,"idx":r.get::<_,i64>(1)?,
                "type":r.get::<_,String>(2)?,"iteration":r.get::<_,Option<i64>>(3)?,
                "call_id":r.get::<_,Option<String>>(4)?,"payload":payload,"has_raw":r.get::<_,bool>(6)?}))
        })?;
        Ok(Some(
            json!({"turn":turn,"items":rows.collect::<std::result::Result<Vec<_>,_>>()?}),
        ))
    }

    pub fn admin_raw(&self, turn_id: &str, item_id: &str) -> Result<Option<Value>> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT raw_json FROM items WHERE id=?1 AND turn_id=?2 AND item_type='function_call_output'",
            params![item_id,turn_id],|r|r.get::<_,Option<String>>(0),
        ).optional()?.flatten();
        Ok(raw.map(|raw| json!({"item_id":item_id,"raw":raw})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::{Item, TurnStatus};

    fn user(store: &SqliteStore, id: &str) {
        store.conn.execute("INSERT INTO users(id,username_normalized,display_name,password_hash,created_at,updated_at)
            VALUES (?1,?1,?1,'not-a-real-hash',0,0)",[id]).unwrap();
    }

    #[test]
    fn admin_reads_all_statuses_and_pages_without_loading_raw() {
        let mut st = SqliteStore::in_memory().unwrap();
        user(&st, "alice");
        user(&st, "bob");
        let sid = st.create_session(None, Some("alice")).unwrap();
        let other = st.create_session(None, Some("bob")).unwrap();
        let items = vec![
            Item::user_message(1, "问题"),
            Item::assistant_message_full(2, 1, "我先查一下", "思考字段"),
            Item::function_call(3, 1, "call-1", "search_videos", &json!({"query":"科普"})),
            Item::function_call_output_full(
                4,
                1,
                "call-1",
                "失败的完整内容",
                true,
                Some("{\"raw\":true}".into()),
                Some("search"),
                Some(0),
            ),
        ];
        st.save_turn(&sid, "test", TurnStatus::Failed("上游失败".into()), &items)
            .unwrap();
        st.save_turn(
            &sid,
            "test",
            TurnStatus::Cancelled,
            &[Item::user_message(1, "停止")],
        )
        .unwrap();
        let list = st.admin_sessions("alice", 1, 0).unwrap();
        assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
        assert_ne!(list["sessions"][0]["id"], other);
        let first = st.admin_turns(&sid, 1, 0).unwrap().unwrap();
        assert_eq!(first["turns"][0]["status"], "failed");
        assert_eq!(first["has_more"], true);
        let second = st.admin_turns(&sid, 1, 1).unwrap().unwrap();
        assert_eq!(second["turns"][0]["status"], "cancelled");
        assert_eq!(second["has_more"], false);
        let tid = first["turns"][0]["id"].as_str().unwrap();
        let detail = st.admin_turn(tid).unwrap().unwrap();
        assert_eq!(detail["items"][1]["payload"]["reasoning"], "思考字段");
        assert_eq!(detail["items"][3]["has_raw"], true);
        assert!(detail["items"][3].get("raw").is_none());
        let iid = detail["items"][3]["id"].as_str().unwrap();
        assert_eq!(
            st.admin_raw(tid, iid).unwrap().unwrap()["raw"],
            "{\"raw\":true}"
        );
        let wrong = second["turns"][0]["id"].as_str().unwrap();
        assert!(st.admin_raw(wrong, iid).unwrap().is_none());
        assert!(st.admin_turn("missing").unwrap().is_none());
        assert!(st.admin_turns("missing", 30, 0).unwrap().is_none());
    }
}

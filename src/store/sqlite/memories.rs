//! 长期记忆的存取。表结构和每一列的含义见 `migrations/agent/010_memories.sql`，
//! 记什么、怎么核对见 `agent::memory`。
//!
//! 是 `sqlite` 的子模块，所以能直接用那边的连接和 `write_tx`。
//!
//! ★ 和会话那批查询一样，**每一条都带 user_id**，写在 SQL 的 WHERE 里。
//!   「拿别人的记忆 id 来删」删不掉，「拿别人的记忆编号来替换」改不到。

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use super::{SqliteStore, write_tx};
use crate::agent::memory::{Category, Memory, MemoryOp, squash};
use crate::content::model::{new_id, now_ts};
use crate::error::Result;

/// 一次落库实际做了什么。写日志用，测试也靠它断言。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub added: usize,
    pub replaced: usize,
    /// 又被提到一次（含「新增了一句一模一样的」）
    pub confirmed: usize,
    /// 一类满了被挤掉的条数。真删
    pub evicted: usize,
    /// 没执行的：要替换 / 确认的那条已经不在了（刚被用户删掉、不是这个用户的），
    /// 或者这一类满了而且这一批全是新的、挤不动
    pub skipped: usize,
}

/// 生效中的记忆。顺序：身份 → 偏好 → 别再推荐，同类里最近被提到的在前。
/// 拼上下文、面板列表、抽取时给模型看的已有记忆，用的都是这个顺序。
fn active(conn: &Connection, user_id: &str) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, category, content, evidence, created_at, updated_at
         FROM memories
         WHERE user_id = ?1 AND status = 'active'
         ORDER BY CASE category WHEN 'profile' THEN 0 WHEN 'preference' THEN 1 ELSE 2 END,
                  updated_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![user_id], |r| {
        let cat: String = r.get(1)?;
        Ok(Memory {
            id: r.get(0)?,
            // 库里有 CHECK 约束，走不到 unwrap_or；真走到了也别让一行脏数据
            // 把整个会话拖垮
            category: Category::parse(&cat).unwrap_or(Category::Preference),
            content: r.get(2)?,
            evidence: r.get(3)?,
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

impl SqliteStore {
    /// 这个用户生效中的记忆。
    pub fn active_memories(&self, user_id: &str) -> Result<Vec<Memory>> {
        active(&self.conn, user_id)
    }

    /// 会话属于谁。命令行建的会话没有主人，返回 None——这些会话不读也不写记忆。
    pub fn session_owner(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT user_id FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// 这个会话要带的那段记忆。第一次调用时生成并冻结，之后一直返回同一段。
    ///
    /// 返回 None：会话没有主人，或者生成那一刻这个用户没有记忆。
    ///
    /// `render` 把记忆拼成文字（`agent::memory::render_block`），由调用方给，
    /// 这里只管「什么时候生成、存在哪」。
    ///
    /// ★ 为什么冻结：记忆排在发给模型的内容最前面。每轮按最新的记忆重算的话，
    ///   记忆一变，后面整段对话历史都要按原价重算（DeepSeek 命中前缀缓存的
    ///   输入便宜约 30 倍）。Laplace 踩过这个坑，也是按会话冻结。
    pub fn frozen_memory_context(
        &mut self,
        session_id: &str,
        render: impl FnOnce(&[Memory]) -> String,
    ) -> Result<Option<String>> {
        // 快路径：已经冻结过就直接用，不拿写锁
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT user_id, memory_context FROM sessions WHERE id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(owner), frozen)) = row else {
            return Ok(None);
        };
        if let Some(text) = frozen {
            return Ok(non_empty(text));
        }

        // ★ 慢路径在写事务里：再查一次、生成、写回，三步不能被打断。
        //   和 delete_memory 互斥——不然「读到记忆 → 用户删了一条、清了快照 →
        //   这里写回快照」会把刚删掉的那条冻进去，删了还在影响回答。
        let tx = write_tx(&mut self.conn)?;
        let frozen: Option<String> = tx.query_row(
            "SELECT memory_context FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        if let Some(text) = frozen {
            return Ok(non_empty(text));
        }
        let text = render(&active(&tx, &owner)?);
        tx.execute(
            "UPDATE sessions SET memory_context = ?2 WHERE id = ?1",
            params![session_id, text],
        )?;
        tx.commit()?;
        Ok(non_empty(text))
    }

    /// 这个会话里用户之前问过的问题，按时间顺序，最多 `limit` 条，不含 `exclude_turn`。
    ///
    /// 抽取时当参考给模型看。只取 user_message：那一类条目里只有用户的原话
    /// （停止后的提示、压缩摘要都是读历史时临时加的，不落库）。
    pub fn recent_user_questions(
        &self,
        session_id: &str,
        exclude_turn: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.payload_json FROM items i JOIN turns t ON t.id = i.turn_id
             WHERE t.session_id = ?1 AND t.id != ?2 AND i.item_type = 'user_message'
             ORDER BY t.seq DESC, i.idx DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, exclude_turn, limit as i64], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out: Vec<String> = rows
            .filter_map(std::result::Result::ok)
            .filter_map(|p| {
                serde_json::from_str::<serde_json::Value>(&p)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
            })
            .collect();
        out.reverse();
        Ok(out)
    }

    /// 执行一批核对过的记忆操作。**一个事务**：要么全做，要么全没做。
    ///
    /// `source_turn_id` 是这批记忆出自哪一轮。
    pub fn apply_memory_ops(
        &mut self,
        user_id: &str,
        source_turn_id: Option<&str>,
        ops: &[MemoryOp],
    ) -> Result<ApplyReport> {
        let now = now_ts();
        let tx = write_tx(&mut self.conn)?;
        let mut rep = ApplyReport::default();
        // 这一批里新建或刷新过的。满了要挤的时候不挤它们
        let mut touched: HashSet<String> = HashSet::new();

        let insert = |tx: &Connection, category: Category, content: &str, evidence: &str| {
            let id = new_id();
            tx.execute(
                "INSERT INTO memories
                     (id, user_id, category, content, evidence, source_turn_id,
                      status, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,'active',?7,?7)",
                params![
                    id,
                    user_id,
                    category.as_str(),
                    content,
                    evidence,
                    source_turn_id,
                    now
                ],
            )?;
            Ok::<_, crate::error::ClipKnowError>(id)
        };
        let touch = |tx: &Connection, id: &str| {
            tx.execute(
                "UPDATE memories SET updated_at = ?3
                 WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
                params![id, user_id, now],
            )
        };

        for op in ops {
            match op {
                MemoryOp::Add {
                    category,
                    content,
                    evidence,
                } => {
                    // 同一类里已经有意思一样的一条（去掉空白后相同）：当成又说了一遍
                    let same = active(&tx, user_id)?
                        .into_iter()
                        .find(|m| m.category == *category && squash(&m.content) == squash(content));
                    if let Some(m) = same {
                        touch(&tx, &m.id)?;
                        touched.insert(m.id);
                        rep.confirmed += 1;
                        continue;
                    }
                    let n: i64 = tx.query_row(
                        "SELECT count(*) FROM memories
                         WHERE user_id = ?1 AND category = ?2 AND status = 'active'",
                        params![user_id, category.as_str()],
                        |r| r.get(0),
                    )?;
                    if n as usize >= category.cap() {
                        // ★ 满了：挤掉这一类里最久没被提到的那条。真删——它不是被
                        //   新说法取代，留着就成了面板上看不到、也删不掉的数据。
                        //   这一批里刚动过的不挤（不然可能刚确认的又被挤掉）。
                        let mut stmt = tx.prepare(
                            "SELECT id FROM memories
                             WHERE user_id = ?1 AND category = ?2 AND status = 'active'
                             ORDER BY updated_at ASC, id ASC",
                        )?;
                        let victim: Option<String> = stmt
                            .query_map(params![user_id, category.as_str()], |r| {
                                r.get::<_, String>(0)
                            })?
                            .filter_map(std::result::Result::ok)
                            .find(|id| !touched.contains(id));
                        drop(stmt);
                        match victim {
                            Some(v) => {
                                tx.execute("DELETE FROM memories WHERE id = ?1", params![v])?;
                                rep.evicted += 1;
                            }
                            None => {
                                rep.skipped += 1;
                                continue;
                            }
                        }
                    }
                    let id = insert(&tx, *category, content, evidence)?;
                    touched.insert(id);
                    rep.added += 1;
                }
                MemoryOp::Replace {
                    target,
                    content,
                    evidence,
                } => {
                    let old: Option<(String, String)> = tx
                        .query_row(
                            "SELECT category, content FROM memories
                             WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
                            params![target, user_id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    // 不在了：这一批开始之前用户刚把它删掉，或者根本不是这个人的
                    let Some((cat, old_content)) = old else {
                        rep.skipped += 1;
                        continue;
                    };
                    // 换汤不换药：内容其实没变，就只刷新时间，不多出一个版本
                    if squash(&old_content) == squash(content) {
                        touch(&tx, target)?;
                        touched.insert(target.clone());
                        rep.confirmed += 1;
                        continue;
                    }
                    let category = Category::parse(&cat).unwrap_or(Category::Preference);
                    let id = insert(&tx, category, content, evidence)?;
                    tx.execute(
                        "UPDATE memories SET status = 'superseded', superseded_by = ?2
                         WHERE id = ?1",
                        params![target, id],
                    )?;
                    touched.insert(id);
                    rep.replaced += 1;
                }
                MemoryOp::Confirm { target } => {
                    if touch(&tx, target)? == 1 {
                        touched.insert(target.clone());
                        rep.confirmed += 1;
                    } else {
                        rep.skipped += 1;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(rep)
    }

    /// 用户在面板上删掉一条记忆。删到了返回 true。
    ///
    /// 真删，旧版本跟着删（`superseded_by` 上的 ON DELETE CASCADE）。
    ///
    /// ★ 同一个事务里把这个用户**所有会话**冻结的记忆段清掉，下一轮重新生成。
    ///   不清的话，已经开着的会话会一直带着这条——删了还在影响回答。
    ///   代价是那些会话下一轮的前缀缓存失效一次，删除很少发生，付得起。
    pub fn delete_memory(&mut self, user_id: &str, memory_id: &str) -> Result<bool> {
        let tx = write_tx(&mut self.conn)?;
        let n = tx.execute(
            "DELETE FROM memories WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
            params![memory_id, user_id],
        )?;
        if n == 0 {
            // 不存在 / 不是你的 / 只是一个旧版本：一律没删到，事务回滚
            return Ok(false);
        }
        tx.execute(
            "UPDATE sessions SET memory_context = NULL WHERE user_id = ?1",
            params![user_id],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::{Item, TurnStatus};

    fn store() -> SqliteStore {
        let mut s = SqliteStore::in_memory().unwrap();
        for id in ["u1", "u2"] {
            s.exec_for_test(&format!(
                "INSERT INTO users (id, username_normalized, display_name, password_hash,
                                    status, created_at, updated_at)
                 VALUES ('{id}','{id}','{id}','x','active',0,0)"
            ))
            .unwrap();
        }
        s
    }

    fn add(cat: Category, content: &str) -> MemoryOp {
        MemoryOp::Add {
            category: cat,
            content: content.into(),
            evidence: content.into(),
        }
    }

    fn contents(s: &SqliteStore, user: &str) -> Vec<String> {
        s.active_memories(user)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect()
    }

    /// 把某条记忆的「最后被提到」时间改到很久以前，好测挤掉的顺序。
    fn age(s: &mut SqliteStore, content: &str, ts: i64) {
        s.exec_for_test(&format!(
            "UPDATE memories SET updated_at = {ts} WHERE content = '{content}'"
        ))
        .unwrap();
    }

    #[test]
    fn 新增的记忆按类别和最近程度列出来_只列自己的() {
        let mut s = store();
        s.apply_memory_ops(
            "u1",
            None,
            &[
                add(Category::Exclusion, "@a 合作过"),
                add(Category::Preference, "只要中文"),
                add(Category::Profile, "做美妆"),
            ],
        )
        .unwrap();
        s.apply_memory_ops("u2", None, &[add(Category::Preference, "波的偏好")])
            .unwrap();
        assert_eq!(contents(&s, "u1"), vec!["做美妆", "只要中文", "@a 合作过"]);
        assert_eq!(contents(&s, "u2"), vec!["波的偏好"]);
    }

    #[test]
    fn 说了一遍一样的话_不多出一条_只刷新时间() {
        let mut s = store();
        s.apply_memory_ops("u1", None, &[add(Category::Preference, "只要中文")])
            .unwrap();
        age(&mut s, "只要中文", 1);
        let rep = s
            .apply_memory_ops("u1", None, &[add(Category::Preference, "只要 中文")])
            .unwrap();
        assert_eq!(rep.confirmed, 1, "{rep:?}");
        let ms = s.active_memories("u1").unwrap();
        assert_eq!(ms.len(), 1);
        assert!(ms[0].updated_at > 1, "时间该刷新");
    }

    #[test]
    fn 改口是替换_旧的标成被取代并指向新的() {
        let mut s = store();
        s.apply_memory_ops("u1", None, &[add(Category::Preference, "只要中文")])
            .unwrap();
        let old = s.active_memories("u1").unwrap()[0].id.clone();
        let rep = s
            .apply_memory_ops(
                "u1",
                None,
                &[MemoryOp::Replace {
                    target: old.clone(),
                    content: "中文英文都要".into(),
                    evidence: "英文也行".into(),
                }],
            )
            .unwrap();
        assert_eq!(rep.replaced, 1);
        let now = s.active_memories("u1").unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].content, "中文英文都要");
        assert_eq!(now[0].category, Category::Preference, "类别沿用旧的");
        let n = s
            .count_for_test(&format!(
                "SELECT count(*) FROM memories
                 WHERE id = '{old}' AND status = 'superseded' AND superseded_by = '{}'",
                now[0].id
            ))
            .unwrap();
        assert_eq!(n, 1, "旧的该留着、标成被取代、指向新的");
    }

    /// ★ 拿别人的记忆 id 来替换、确认、删除，一律不生效。
    #[test]
    fn 别人的记忆改不到也删不到() {
        let mut s = store();
        s.apply_memory_ops("u2", None, &[add(Category::Preference, "波的偏好")])
            .unwrap();
        let bobs = s.active_memories("u2").unwrap()[0].id.clone();
        let rep = s
            .apply_memory_ops(
                "u1",
                None,
                &[
                    MemoryOp::Replace {
                        target: bobs.clone(),
                        content: "阿改的".into(),
                        evidence: "x".into(),
                    },
                    MemoryOp::Confirm {
                        target: bobs.clone(),
                    },
                ],
            )
            .unwrap();
        assert_eq!(rep.skipped, 2, "{rep:?}");
        assert!(!s.delete_memory("u1", &bobs).unwrap());
        assert_eq!(contents(&s, "u2"), vec!["波的偏好"]);
        assert!(contents(&s, "u1").is_empty());
    }

    #[test]
    fn 满了挤掉最久没被提到的那条_真删() {
        let mut s = store();
        let cap = Category::Profile.cap();
        let ops: Vec<MemoryOp> = (0..cap)
            .map(|i| add(Category::Profile, &format!("身份{i}")))
            .collect();
        s.apply_memory_ops("u1", None, &ops).unwrap();
        // 身份1 最久没被提到
        for i in 0..cap {
            age(&mut s, &format!("身份{i}"), if i == 1 { 1 } else { 100 });
        }
        let rep = s
            .apply_memory_ops("u1", None, &[add(Category::Profile, "新身份")])
            .unwrap();
        assert_eq!((rep.added, rep.evicted), (1, 1), "{rep:?}");
        let now = contents(&s, "u1");
        assert_eq!(now.len(), cap);
        assert!(!now.contains(&"身份1".to_string()), "{now:?}");
        assert_eq!(
            s.count_for_test("SELECT count(*) FROM memories WHERE content = '身份1'")
                .unwrap(),
            0,
            "挤掉是真删，不是改状态"
        );
    }

    #[test]
    fn 这一批刚确认过的不会被同一批挤掉() {
        let mut s = store();
        let cap = Category::Profile.cap();
        let ops: Vec<MemoryOp> = (0..cap)
            .map(|i| add(Category::Profile, &format!("身份{i}")))
            .collect();
        s.apply_memory_ops("u1", None, &ops).unwrap();
        for i in 0..cap {
            age(&mut s, &format!("身份{i}"), i as i64 + 1);
        }
        // 身份0 最老，但这一批先确认了它，再加一条新的：该挤的是身份1
        let oldest = s
            .active_memories("u1")
            .unwrap()
            .into_iter()
            .find(|m| m.content == "身份0")
            .unwrap()
            .id;
        s.apply_memory_ops(
            "u1",
            None,
            &[
                MemoryOp::Confirm { target: oldest },
                add(Category::Profile, "新身份"),
            ],
        )
        .unwrap();
        let now = contents(&s, "u1");
        assert!(now.contains(&"身份0".to_string()), "{now:?}");
        assert!(!now.contains(&"身份1".to_string()), "{now:?}");
    }

    #[test]
    fn 删除是真删_旧版本跟着删_而且清掉这个用户所有会话的冻结段() {
        let mut s = store();
        s.apply_memory_ops("u1", None, &[add(Category::Preference, "只要中文")])
            .unwrap();
        let v1 = s.active_memories("u1").unwrap()[0].id.clone();
        s.apply_memory_ops(
            "u1",
            None,
            &[MemoryOp::Replace {
                target: v1,
                content: "中文英文都要".into(),
                evidence: "x".into(),
            }],
        )
        .unwrap();
        let v2 = s.active_memories("u1").unwrap()[0].id.clone();

        let a = s.create_session(None, Some("u1")).unwrap();
        let b = s.create_session(None, Some("u2")).unwrap();
        s.frozen_memory_context(&a, |_| "阿的冻结段".into())
            .unwrap();
        s.frozen_memory_context(&b, |_| "波的冻结段".into())
            .unwrap();

        assert!(s.delete_memory("u1", &v2).unwrap());
        assert_eq!(
            s.count_for_test("SELECT count(*) FROM memories WHERE user_id = 'u1'")
                .unwrap(),
            0,
            "新旧两个版本都该删掉"
        );
        assert_eq!(
            s.count_for_test(&format!(
                "SELECT count(*) FROM sessions WHERE id = '{a}' AND memory_context IS NULL"
            ))
            .unwrap(),
            1,
            "阿的会话要重新生成"
        );
        assert_eq!(
            s.frozen_memory_context(&b, |_| unreachable!()).unwrap(),
            Some("波的冻结段".into()),
            "别人的会话不该被动"
        );
        assert!(!s.delete_memory("u1", &v2).unwrap(), "删第二次没东西可删");
    }

    #[test]
    fn 冻结段第一次生成_之后一直是同一段_新记忆要等新会话() {
        let mut s = store();
        s.apply_memory_ops("u1", None, &[add(Category::Preference, "只要中文")])
            .unwrap();
        let sid = s.create_session(None, Some("u1")).unwrap();
        let render = |ms: &[Memory]| {
            ms.iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("|")
        };
        let first = s.frozen_memory_context(&sid, render).unwrap();
        assert_eq!(first.as_deref(), Some("只要中文"));

        s.apply_memory_ops("u1", None, &[add(Category::Profile, "做美妆")])
            .unwrap();
        assert_eq!(
            s.frozen_memory_context(&sid, render).unwrap(),
            first,
            "同一个会话里不变"
        );
        let fresh = s.create_session(None, Some("u1")).unwrap();
        assert_eq!(
            s.frozen_memory_context(&fresh, render).unwrap().as_deref(),
            Some("做美妆|只要中文"),
            "新会话拿到新的"
        );
    }

    #[test]
    fn 没主人的会话和没记忆的用户_都不带记忆段() {
        let mut s = store();
        let cli = s.create_session(None, None).unwrap();
        assert_eq!(
            s.frozen_memory_context(&cli, |_| unreachable!()).unwrap(),
            None
        );
        assert_eq!(s.session_owner(&cli).unwrap(), None);

        let sid = s.create_session(None, Some("u1")).unwrap();
        assert_eq!(
            s.frozen_memory_context(&sid, |_| String::new()).unwrap(),
            None
        );
        // 空也冻结：之后不再每轮去查
        assert_eq!(
            s.frozen_memory_context(&sid, |_| unreachable!()).unwrap(),
            None
        );
        assert_eq!(s.session_owner(&sid).unwrap().as_deref(), Some("u1"));
    }

    #[test]
    fn 之前的问题按时间顺序_不含这一轮() {
        let mut s = store();
        let sid = s.create_session(None, Some("u1")).unwrap();
        for q in ["第一问", "第二问"] {
            s.save_turn(&sid, "m", TurnStatus::Done, &[Item::user_message(1, q)])
                .unwrap();
        }
        s.begin_turn("t3", &sid, "m", "第三问", "第三问", None)
            .unwrap();
        assert_eq!(
            s.recent_user_questions(&sid, "t3", 5).unwrap(),
            vec!["第一问".to_string(), "第二问".to_string()]
        );
        assert_eq!(
            s.recent_user_questions(&sid, "t3", 1).unwrap(),
            vec!["第二问".to_string()],
            "只取最近的"
        );
    }

    /// 出处那一轮没了，记忆本身还在。
    #[test]
    fn 删会话不影响记忆本身() {
        let mut s = store();
        let sid = s.create_session(None, Some("u1")).unwrap();
        let turn = s
            .save_turn(&sid, "m", TurnStatus::Done, &[Item::user_message(1, "q")])
            .unwrap();
        s.apply_memory_ops("u1", Some(&turn), &[add(Category::Preference, "只要中文")])
            .unwrap();
        s.delete_session(&sid).unwrap();
        assert_eq!(contents(&s, "u1"), vec!["只要中文"]);
    }
}

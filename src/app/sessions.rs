//! 会话的查询与创建。
//!
//! 这些函数以前是**Go 那边**的 SQL（`web/store.go` 里的 `Sessions()` 和
//! `History()`）。搬过来的原因是「一份真相」：`sessions` / `turns` / `items`
//! 三张表由 Rust 写，Go 只读，于是两边都得懂表结构——改一列要改两处，
//! 漏一处就是线上 bug。搬过来之后 Go 一行聊天表的 SQL 都不写。

use serde::Serialize;

use crate::error::Result;
use crate::store::sqlite::SqliteStore;

/// 会话列表里的一条。
///
/// **不是 `sessions` 表的行**：user_id、deleted_at、creation_request_id
/// 都不出现在这里。给浏览器的东西和库里存的东西是两回事，混成一个的话，
/// 加一列内部字段就会直接漏到接口上。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    /// 没标题时是空串，不是 null。
    /// 前端写的是 `s.title || '(还没有标题)'`，null 和空串都能兜住，
    /// 但空串让「这个字段永远是字符串」成立，少一类判断。
    pub title: String,
    pub created_at: i64,
    /// 这个会话里有一轮正在跑。会话列表上打个标记，并发时一眼看出哪几个在忙。
    pub running: bool,
}

/// 聊天记录里的一条。
///
/// 数据库里一次提问存 4~5 条（问题 / 中间思考 / 工具调用 / 工具结果 /
/// 最终答案），这里只留两条：问题和最终答案。中间那些是过程，不是对话。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UiMessage {
    /// "user" 或 "assistant"
    pub role: &'static str,
    pub text: String,
    /// 第几次提问。
    pub seq: i64,
    /// 这一轮是失败收场的（截断 / 撞迭代上限 / 协议异常）。
    ///
    /// ⚠️ 只有布尔，没有原因——`turns.status` 只存 "done"/"failed"，
    /// Rust 侧 `TurnStatus::Failed(String)` 里那个原因**没落库**。实时看
    /// 的时候能从 done 事件的 note 里看到，刷新之后就只剩这个布尔了。
    /// 要补的话是 turns 加一列 + 一次迁移。
    pub failed: bool,
    /// 这一轮还在跑。只会出现在一条**占位的** assistant 消息上（text 为空）：
    /// 跑的过程中库里只有用户的问题，答案要等跑完才写进去。
    /// 前端看到它就去订阅这一轮的事件流，接着看。
    pub running: bool,
    /// 这一轮跑到一半服务停了（崩溃、被杀）。同样是占位消息。
    pub interrupted: bool,
}

/// 找不到，或者不是你的。
///
/// **两种情况刻意不区分。** 区分开就等于告诉对方「有这么个会话，只是不
/// 给你看」——光是「存在」这件事本身就是泄漏（比如能拿来枚举别人建了
/// 多少个会话）。
#[derive(Debug, PartialEq, Eq)]
pub struct NotFound;

/// 这个用户的会话，最近有活动的排前面。
pub fn list(store: &SqliteStore, user_id: &str, limit: usize) -> Result<Vec<SessionSummary>> {
    let running = store.running_sessions_for_user(user_id)?;
    Ok(store
        .list_sessions_for_user(user_id, limit)?
        .into_iter()
        .map(|s| SessionSummary {
            running: running.contains(&s.id),
            id: s.id,
            title: s.title.unwrap_or_default(),
            created_at: s.created_at,
        })
        .collect())
}

/// 一个会话的聊天记录。不是这个用户的会话返回 [`NotFound`]。
///
/// ★ 归属检查和取数据是**两次查询**，中间有一瞬间的空窗。这里不要紧：
///   会话的归属一旦写入就不再改变（没有转让功能），所以「先确认是你的、
///   再读内容」不会读到别人的东西。真正的并发问题在阶段 C 的执行准入那里，
///   那边用的是事务。
pub fn messages(
    store: &SqliteStore,
    user_id: &str,
    session_id: &str,
) -> Result<std::result::Result<Vec<UiMessage>, NotFound>> {
    // 先确认会话存在且是他的。
    //
    // 只靠下面那条查询（它自己也按 user_id 过滤）是不够的：别人的会话和
    // 空会话都会返回空列表，于是「没有这个会话」会显示成「这个会话是空的」。
    if !store.session_owned_by(user_id, session_id)? {
        return Ok(Err(NotFound));
    }
    let rows = store.ui_items_for_user(user_id, session_id)?;
    Ok(Ok(project(rows)))
}

/// 新建一个属于这个用户的空会话，返回 id。
pub fn create(store: &mut SqliteStore, user_id: &str, title: Option<&str>) -> Result<String> {
    store.create_session(title, Some(user_id))
}

/// 把库里的原始条目投影成界面要显示的消息。
///
/// 规则：每个 turn 取两样——user_message（问的），以及**最后一条**
/// assistant_message（最终答案）。前面那些 assistant_message 是模型在调
/// 工具之前的中间思考，界面上不显示。
///
/// 单独抽成纯函数是为了能离线测这条规则。它是从 Go 的 `History()` 搬过来
/// 的，搬运过程中最容易悄悄改掉语义，而改掉的表现是「界面上多出几句半截
/// 的话」——看起来像模型的问题，不像代码的问题。
fn project(rows: Vec<(i64, String, String, String)>) -> Vec<UiMessage> {
    let mut out: Vec<UiMessage> = Vec::new();
    // 当前 turn 的最后一条 assistant 在 out 里的下标。None = 这个 turn
    // 还没出现过 assistant。用下标覆盖而不是先收集再挑，是为了让
    // 「问题在前、答案在后」的顺序自然保持住。
    let mut last_assistant: Option<usize> = None;
    // 当前 turn：(seq, 状态)
    let mut cur: Option<(i64, String)> = None;

    for (seq, status, kind, text) in rows {
        if cur.as_ref().map(|(s, _)| *s) != Some(seq) {
            close_turn(&mut out, cur.take(), last_assistant);
            cur = Some((seq, status.clone()));
            last_assistant = None;
        }
        match kind.as_str() {
            "user_message" => out.push(UiMessage {
                role: "user",
                text,
                seq,
                failed: false,
                running: false,
                interrupted: false,
            }),
            "assistant_message" => {
                let m = UiMessage {
                    role: "assistant",
                    text,
                    seq,
                    failed: status != "done",
                    running: false,
                    interrupted: false,
                };
                match last_assistant {
                    Some(i) => out[i] = m, // 覆盖掉上一条中间思考
                    None => {
                        last_assistant = Some(out.len());
                        out.push(m);
                    }
                }
            }
            // SQL 里已经按 item_type 过滤过，走不到这里。真走到了就跳过，
            // 不让一条没见过的类型把整个会话变成打不开。
            _ => {}
        }
    }
    close_turn(&mut out, cur, last_assistant);
    out
}

/// 一个 turn 的条目读完了：如果它还在跑、或者被中断了，而且一条答案都
/// 没有，补一条**占位**的 assistant 消息，让界面知道该显示什么。
///
/// 不补的话，界面上只有一句孤零零的问题，看不出是「还在答」「断了」
/// 还是「答完了但什么都没说」。
fn close_turn(
    out: &mut Vec<UiMessage>,
    turn: Option<(i64, String)>,
    last_assistant: Option<usize>,
) {
    let Some((seq, status)) = turn else { return };
    if last_assistant.is_some() {
        return;
    }
    let (running, interrupted) = match status.as_str() {
        "running" => (true, false),
        "interrupted" => (false, true),
        _ => return,
    };
    out.push(UiMessage {
        role: "assistant",
        text: String::new(),
        seq,
        failed: interrupted,
        running,
        interrupted,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::{Item, ItemKind, TurnStatus};
    use serde_json::json;

    fn row(seq: i64, done: bool, kind: &str, text: &str) -> (i64, String, String, String) {
        let st = if done { "done" } else { "failed" };
        (seq, st.into(), kind.into(), text.into())
    }

    fn row_st(seq: i64, status: &str, kind: &str, text: &str) -> (i64, String, String, String) {
        (seq, status.into(), kind.into(), text.into())
    }

    /// ★ 跑到一半的那一轮：问题照常显示，后面跟一条「正在回答」的占位。
    ///
    /// 这就是「刷新后问题全不见了」那个 bug 的界面这一半：库里有了问题，
    /// 界面还得知道它**还在跑**，才会去订阅事件流接着看。
    #[test]
    fn 正在跑的一轮显示问题和一条正在回答的占位() {
        let got = project(vec![
            row(1, true, "user_message", "问题一"),
            row(1, true, "assistant_message", "答案一"),
            row_st(2, "running", "user_message", "问题二"),
        ]);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[2].text, "问题二");
        assert!(got[3].running && got[3].text.is_empty(), "{:?}", got[3]);
        assert_eq!(got[3].seq, 2);
        assert!(!got[1].running, "答完的那轮不该标成在跑");
    }

    #[test]
    fn 被中断的一轮显示问题和一条中断占位() {
        let got = project(vec![row_st(
            1,
            "interrupted",
            "user_message",
            "问到一半服务重启了",
        )]);
        assert_eq!(got.len(), 2);
        assert!(got[1].interrupted && got[1].failed && !got[1].running);
    }

    /// 中断占位只在**没有任何答案**时补。有半截答案的话用它就行，
    /// 标成失败——不能再多塞一条空的。
    #[test]
    fn 有答案的轮次不补占位() {
        let got = project(vec![
            row_st(1, "interrupted", "user_message", "问"),
            row_st(1, "interrupted", "assistant_message", "半截"),
        ]);
        assert_eq!(got.len(), 2);
        assert!(got[1].failed && got[1].text == "半截");
    }

    #[test]
    fn 一轮里只留最后一条助手消息() {
        let got = project(vec![
            row(1, true, "user_message", "这条视频讲什么"),
            row(1, true, "assistant_message", "我来看看这条视频。"), // 中间思考
            row(1, true, "assistant_message", "它讲的是 A。"),       // 最终答案
        ]);
        assert_eq!(got.len(), 2, "中间思考没被盖掉: {got:?}");
        assert_eq!(got[0].role, "user");
        assert_eq!(got[1].text, "它讲的是 A。");
    }

    #[test]
    fn 问题永远排在答案前面() {
        let got = project(vec![
            row(1, true, "user_message", "问题一"),
            row(1, true, "assistant_message", "中间"),
            row(1, true, "assistant_message", "答案一"),
            row(2, true, "user_message", "问题二"),
            row(2, true, "assistant_message", "答案二"),
        ]);
        let seq: Vec<_> = got.iter().map(|m| (m.role, m.text.as_str())).collect();
        assert_eq!(
            seq,
            vec![
                ("user", "问题一"),
                ("assistant", "答案一"),
                ("user", "问题二"),
                ("assistant", "答案二"),
            ]
        );
    }

    #[test]
    fn 失败的那一轮被标出来_而且只标那一轮() {
        let got = project(vec![
            row(1, true, "user_message", "问题一"),
            row(1, true, "assistant_message", "答案一"),
            row(2, false, "user_message", "问题二"),
            row(2, false, "assistant_message", "半截话"),
        ]);
        assert!(!got[1].failed, "第一轮是成功的，不该标失败");
        assert!(got[3].failed, "第二轮失败了，没标出来");
        // 用户自己的问题不标失败——失败的是回答，不是提问
        assert!(!got[2].failed);
    }

    #[test]
    fn 一条助手消息都没有的轮次也不会丢掉问题() {
        // 模型调用直接失败的情况：问题存了，答案没有
        let got = project(vec![row(1, false, "user_message", "问了但没答上")]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].role, "user");
    }

    // ── 下面几条要真库，因为它们验的是 SQL 里的归属过滤 ──

    /// 插一个用户。`users` 表是 Go 拥有的，Rust 生产代码不写它——
    /// 但 sessions.user_id 有外键指过去，不插就建不出会话。
    fn add_user(store: &mut SqliteStore, id: &str) {
        store
            .exec_for_test(&format!(
                "INSERT INTO users (id, username_normalized, display_name, password_hash,
                                    status, created_at, updated_at)
                 VALUES ('{id}','{id}','{id}','x','active',0,0)"
            ))
            .unwrap();
    }

    /// 造一个有内容的会话，返回 (store, session_id)。
    fn seeded(user: &str, question: &str, answer: &str) -> (SqliteStore, String) {
        let mut store = SqliteStore::in_memory().unwrap();
        // sessions.user_id 有外键指向 users，先插一个真用户
        add_user(&mut store, user);
        let sid = create(&mut store, user, Some("测试会话")).unwrap();
        store
            .save_turn(
                &sid,
                "fake",
                TurnStatus::Done,
                &[
                    Item {
                        idx: 0,
                        kind: ItemKind::UserMessage,
                        iteration: None,
                        call_id: None,
                        payload: json!({"text": question}),
                        raw_json: None,
                    },
                    Item {
                        idx: 1,
                        kind: ItemKind::AssistantMessage,
                        iteration: Some(1),
                        call_id: None,
                        payload: json!({"text": answer}),
                        raw_json: None,
                    },
                ],
            )
            .unwrap();
        (store, sid)
    }

    #[test]
    fn 自己的会话读得到() {
        let (store, sid) = seeded("u_alice", "问题", "答案");
        let msgs = messages(&store, "u_alice", &sid).unwrap().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].text, "答案");
    }

    /// ★ 越权读必须是「未找到」，不是「空会话」。
    ///
    /// 返回空列表的话，界面上显示成「这个会话是空的」——对方由此知道
    /// **这个 id 确实存在**，只是没内容。而真相是它存在且有内容。
    #[test]
    fn 别人的会话按未找到处理() {
        let (mut store, sid) = seeded("u_alice", "问题", "答案");
        add_user(&mut store, "u_bob");
        assert_eq!(messages(&store, "u_bob", &sid).unwrap(), Err(NotFound));
    }

    #[test]
    fn 不存在的会话也是未找到() {
        let (store, _) = seeded("u_alice", "问题", "答案");
        assert_eq!(
            messages(&store, "u_alice", "根本没有这个 id").unwrap(),
            Err(NotFound)
        );
    }

    #[test]
    fn 列表只出自己的会话() {
        let (mut store, _) = seeded("u_alice", "问题", "答案");
        add_user(&mut store, "u_bob");
        create(&mut store, "u_bob", Some("鲍勃的会话")).unwrap();

        let alice = list(&store, "u_alice", 100).unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].title, "测试会话");
        let bob = list(&store, "u_bob", 100).unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].title, "鲍勃的会话");
    }

    /// ★ 列表按「最后有活动」排，不是按「什么时候建的」。
    ///
    /// 昨天建的会话今天聊了一下午，该排在今天刚建、一句没说的那个前面。
    /// 靠 save_turn 顺手更新 sessions.updated_at 实现——这条测试钉住那件事，
    /// 因为漏了它不会报错，只会让列表顺序看起来"有点怪"。
    #[test]
    fn 列表按最后活动时间排_不是按创建时间() {
        let (mut store, old_sid) = seeded("u_alice", "问题", "答案");
        // 后建的一个，一句话都没说
        let new_sid = create(&mut store, "u_alice", Some("新建的")).unwrap();

        // ★ 把两个会话的时间戳显式拉开。
        //
        //   now_ts() 是**秒**级，而这个测试从头到尾跑不到一毫秒——不这么做的话
        //   两个会话的 created_at 一模一样，排序落到 id 的兜底比较上，测出来的
        //   就不是「按活动时间排」这件事了。（第一版就是这么假红的。）
        store
            .exec_for_test(&format!(
                "UPDATE sessions SET created_at=100, updated_at=100 WHERE id='{old_sid}'"
            ))
            .unwrap();
        store
            .exec_for_test(&format!(
                "UPDATE sessions SET created_at=200, updated_at=200 WHERE id='{new_sid}'"
            ))
            .unwrap();

        let before = list(&store, "u_alice", 10).unwrap();
        assert_eq!(before[0].id, new_sid, "后建的该排最前");

        // 在老会话里再问一轮 → 它应该顶上来
        store
            .save_turn(
                &old_sid,
                "fake",
                TurnStatus::Done,
                &[Item {
                    idx: 0,
                    kind: ItemKind::UserMessage,
                    iteration: None,
                    call_id: None,
                    payload: json!({"text": "又问了一句"}),
                    raw_json: None,
                }],
            )
            .unwrap();

        let after = list(&store, "u_alice", 10).unwrap();
        assert_eq!(
            after[0].id, old_sid,
            "老会话刚聊过，却没排到最前——save_turn 忘了更新 sessions.updated_at"
        );
    }
}

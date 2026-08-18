//! 造一个 turn 的真实数据，看看 items 表里每行到底装什么。
use clipknow::content::model::{Item, TurnStatus};
use clipknow::store::sqlite::SqliteStore;
use serde_json::json;

fn main() {
    let mut st = SqliteStore::in_memory().unwrap();
    let sid = st.create_session(Some("演示")).unwrap();
    let big_raw = format!(r#"{{"videos":[{}]}}"#, "\"...\",".repeat(8000));
    let rendered = "[结果：20 条]\n1. 【千萬不要吃】人類不能吃生肉的真正原因\n   作者 老高與小茉 (@laogao) | 播放 4780929 ...".repeat(60);

    st.save_turn(
        &sid,
        "deepseek-chat",
        TurnStatus::Done,
        &[
            Item::user_message(1, "帮我找几个做科普的 YouTube 博主"),
            Item::assistant_message(2, 1, "我先搜一下。"),
            Item::function_call(
                3,
                1,
                "call_00_A",
                "search_videos",
                &json!({"platform":"youtube","query":"科普"}),
            ),
            Item::function_call_output(4, 1, "call_00_A", &rendered, false, Some(big_raw)),
            Item::assistant_message(5, 2, "结果太分散，我换个词再搜。"),
            Item::function_call(
                6,
                2,
                "call_00_B",
                "search_videos",
                &json!({"platform":"youtube","query":"科学"}),
            ),
            Item::function_call_output(
                7,
                2,
                "call_00_B",
                &rendered,
                false,
                Some("{\"videos\":[]}".into()),
            ),
            Item::assistant_message(8, 3, "推荐毕导THU、言核酱……"),
        ],
    )
    .unwrap();

    println!(
        "{:<5} {:<22} {:>12} {:>12}",
        "idx", "item_type", "payload", "raw_json"
    );
    println!("{}", "─".repeat(56));
    let mut cum_payload = 0usize;
    for it in st.load_history(&sid).unwrap() {
        let p = it.payload.to_string().len();
        cum_payload += p;
        let raw = st
            .get_raw_json(&sid, it.call_id.as_deref().unwrap_or("-"))
            .unwrap();
        println!(
            "{:<5} {:<22} {:>10} B {:>10}",
            it.idx,
            it.kind.as_str(),
            p,
            raw.map(|r| format!("{} B", r.len()))
                .unwrap_or_else(|| "—".into())
        );
    }
    println!("{}", "─".repeat(56));
    println!("payload 合计 {cum_payload} B  ← 这才是发给模型时会累积的东西");
}

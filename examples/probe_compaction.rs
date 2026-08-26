//! 用真 key 验压缩。阈值临时压到很低，不然要聊十几轮才触发。
use clipknow::agent::llm::build_client;
use clipknow::agent::runner::{LoopConfig, run_turn};
use clipknow::content::model::TurnStatus;
use clipknow::ingest::scrapecreators::ScrapeCreators;
use clipknow::store::sqlite::SqliteStore;

fn main() {
    let _ = dotenvy::dotenv();
    let llm = build_client(None).unwrap();
    let api = ScrapeCreators::from_env().unwrap();
    let mut st = SqliteStore::open("/tmp/compact.db").unwrap();
    let sid = st.create_session(Some("压缩验证")).unwrap();
    let cfg = LoopConfig {
        compaction_threshold: 5_000, // 极低，第二轮就会触发
        compaction_target_tokens: 3_000,
        ..LoopConfig::default()
    };

    for q in [
        "毕导THU 这个 YouTube 博主最近在发什么？一句话",
        "他更新频率怎么样",
        "那漫士沉思录呢？一句话",
        "把你已经确认的两个人各用一句话概括，带上粉丝数",
    ] {
        let history = st.load_turns_with_items(&sid).unwrap();
        println!("\n─── 提问：{q}");
        println!(
            "    历史 {} 个 turn，摘要 {}，约 {} token",
            history.turns.len(),
            if history.summary.is_some() {
                "有"
            } else {
                "无"
            },
            history.est_tokens()
        );
        let res = run_turn(&*llm, &api, &mut st, None, &history, q, &cfg);
        st.save_turn(&sid, llm.model_name(), TurnStatus::Done, &res.items)
            .unwrap();
        if let Some((text, upto)) = &res.pending_summary {
            st.save_compaction(&sid, text, *upto).unwrap();
            println!(
                "    ★ 压缩了 {} 次，摘要覆盖到 turn {upto}",
                res.compactions
            );
            println!(
                "    摘要内容：\n{}",
                text.lines()
                    .take(14)
                    .map(|l| format!("      {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        } else {
            println!("    没压缩（{} 次尝试）", res.compactions);
        }
        println!(
            "    {} 轮 · 输入 {} token",
            res.iterations, res.input_tokens
        );
    }
}

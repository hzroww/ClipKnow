//! 用真 key 验三家的翻页。
use clipknow::agent::llm::ToolCall;
use clipknow::agent::tools::{ToolCtx, execute};
use clipknow::ingest::scrapecreators::ScrapeCreators;
use clipknow::store::sqlite::SqliteStore;
use serde_json::json;

fn main() {
    let _ = dotenvy::dotenv();
    let api = ScrapeCreators::from_env().unwrap();
    let mut st = SqliteStore::in_memory().unwrap();
    for (p, h) in [
        ("youtube", "hafu"),
        ("tiktok", "fallontonight"),
        ("instagram", "natgeo"),
    ] {
        for want in [None, Some(60)] {
            let mut args = json!({"platform": p, "handle": h});
            if let Some(w) = want {
                args["max_videos"] = json!(w);
            }
            let out = execute(
                &mut ToolCtx::text_only(&api, &mut st),
                &ToolCall {
                    id: "x".into(),
                    name: "get_creator_videos".into(),
                    args,
                },
            );
            let n = out.result.content.lines().next().unwrap_or("");
            println!(
                "{p:<10} max_videos={:<6} {n}  → {} 页 / {} credit",
                want.map(|w| w.to_string()).unwrap_or_else(|| "无".into()),
                out.external_calls,
                out.credits_charged.unwrap_or(0)
            );
        }
    }
}

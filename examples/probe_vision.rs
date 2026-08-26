//! 真 key 端到端验 fetch_video 的四段材料。
use clipknow::agent::llm::ToolCall;
use clipknow::agent::tools::{ToolCtx, execute};
use clipknow::agent::vision::build_vision_client;
use clipknow::ingest::scrapecreators::ScrapeCreators;
use clipknow::store::sqlite::SqliteStore;
use serde_json::json;

fn main() {
    let _ = dotenvy::dotenv();
    let api = ScrapeCreators::from_env().unwrap();
    let vision = build_vision_client();
    println!(
        "视觉模型: {}",
        vision.as_deref().map_or("未启用", |v| v.model_name())
    );
    let mut st = SqliteStore::open("/tmp/vision_probe.db").unwrap();

    let url = std::env::args().nth(1).expect("给一个视频链接");
    let q = std::env::args().nth(2);
    let mut args = json!({"url": url});
    if let Some(q) = &q {
        args["question"] = json!(q);
    }

    let t = std::time::Instant::now();
    let out = execute(
        &mut ToolCtx {
            api: &api,
            store: &mut st,
            vision: vision.as_deref(),
            vision_budget_left: 3,
        },
        &ToolCall {
            id: "x".into(),
            name: "fetch_video".into(),
            args,
        },
    );
    println!(
        "\n耗时 {:.1}s · SC {} 次 · 视觉 {} 次 · video_tokens {}",
        t.elapsed().as_secs_f32(),
        out.external_calls,
        out.vision_calls,
        out.video_tokens
    );
    println!("is_error = {}", out.result.is_error);
    println!("─────── 给模型的材料 ───────\n{}", out.result.content);
}

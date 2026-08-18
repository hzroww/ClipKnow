//! 拿真 key 把五个工具各打一次，看渲染给模型的文本长什么样。
//! 这是「单元测试全绿 ≠ 真的能调通」的那一道验证。
//!   cargo run --example probe_tools
use clipknow::agent::llm::ToolCall;
use clipknow::agent::tools::execute;
use clipknow::ingest::scrapecreators::ScrapeCreators;
use serde_json::json;

fn main() {
    let _ = dotenvy::dotenv();
    let api = ScrapeCreators::from_env().expect("需要 SCRAPECREATORS_API_KEY");

    let cases = [
        (
            "search_videos",
            json!({"platform":"youtube","query":"科普"}),
        ),
        ("search_videos", json!({"platform":"tiktok","query":"科普"})),
        (
            "search_creators",
            json!({"platform":"instagram","query":"science"}),
        ),
        (
            "get_creator",
            json!({"platform":"youtube","handle":"@thu4878"}),
        ),
        (
            "get_creator_videos",
            json!({"platform":"tiktok","handle":"fallontonight"}),
        ),
        // 应当在打网络之前就被拒
        (
            "search_videos",
            json!({"platform":"instagram","query":"科普"}),
        ),
    ];

    for (name, args) in cases {
        let call = ToolCall {
            id: "probe".into(),
            name: name.into(),
            args: args.clone(),
        };
        let out = execute(&api, &call);
        println!("═══ {name} {args}");
        println!(
            "    is_error={}  endpoint={:?}  原始 {} 字节",
            out.result.is_error,
            out.endpoint,
            out.raw_json.as_ref().map(|s| s.len()).unwrap_or(0)
        );
        let head: String = out
            .result
            .content
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        println!("{head}");
        println!("    → 给模型 {} 字符\n", out.result.content.chars().count());
    }
}

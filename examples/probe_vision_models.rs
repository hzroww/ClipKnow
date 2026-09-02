//! 逐个试哪些视觉模型**真的能收视频**。
//!
//! 「有免费额度」和「能收视频」是两件事——探额度只发了一句文本，
//! 那不能证明它接受 oss:// 引用。
//!
//! ★ 直链一定要用 `parse_play_addr` 解，不要自己写正则。
//!   踩过：自己搜「像 tiktokcdn 的 http 字符串」，抓到的是**封面图**
//!   （p16-common-sign 是图片 CDN，视频在 v16m/v19m 那类主机上），
//!   下载回来 4KB 的 heic，上传上去所有模型都说 Invalid video file，
//!   看起来像是模型的问题，其实一开始就拿错了东西。
//!
//! 只上传一次，同一个引用喂给每个模型，省下重复的下载上传。
use clipknow::agent::vision::{QwenVisionClient, VisionClient};
use clipknow::ingest::discovery::{parse_play_addr, url_still_valid};
use clipknow::ingest::download::HttpVideoFetcher;
use clipknow::ingest::url::Platform;
use clipknow::store::Store;
use clipknow::store::sqlite::SqliteStore;

const MODELS: &[&str] = &[
    "qwen3-vl-flash",
    "qwen3-vl-plus-2025-12-19",
    "qwen-vl-max",
    "qwen-vl-plus",
    "qwen3-omni-flash",
    "qwen3.5-omni-flash",
];

fn main() {
    let _ = dotenvy::dotenv();
    let key = std::env::var("DASHSCOPE_API_KEY").expect("没有 DASHSCOPE_API_KEY");
    let db = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "clipknow.db".into());
    let store = SqliteStore::open(&db).expect("开库失败");
    let now = clipknow::content::model::now_ts();

    // 从库里找一条还没过期的**视频**直链，短的优先
    let mut best: Option<(i64, String, String)> = None;
    for v in store.list_videos(200).expect("列视频失败") {
        if v.platform != Platform::TikTok {
            continue;
        }
        let arts = store.get_artifacts(&v.id).unwrap_or_default();
        for a in &arts {
            let Some(raw) = a.raw_json.as_deref() else {
                continue;
            };
            if let Some(u) = parse_play_addr(Platform::TikTok, raw) {
                let host = u.split('/').nth(2).unwrap_or("?").to_string();
                let dur = v.duration_sec.unwrap_or(9999);
                let fresh = url_still_valid(&u, now);
                println!(
                    "  {dur:>5}s  {host:<34} {}",
                    if fresh { "★未过期" } else { "已过期" }
                );
                if fresh && best.as_ref().is_none_or(|(d, _, _)| dur < *d) {
                    best = Some((dur, u.clone(), host));
                }
            }
        }
    }

    let Some((dur, direct, host)) = best else {
        eprintln!("\n库里没有还没过期的视频直链。先跑一次 fetch_video 换新的。");
        return;
    };
    println!("\n用 {dur} 秒那条，主机 {host}");

    let up = QwenVisionClient::new(
        key.clone(),
        "qwen3-vl-flash".into(),
        HttpVideoFetcher::new(),
    );
    let staged = match up.stage(&direct) {
        Ok(s) => {
            println!("上传 {:.2}MB 成功\n", s.size_bytes as f64 / 1_048_576.0);
            s
        }
        Err(e) => {
            eprintln!("上传失败: {e}");
            return;
        }
    };

    for m in MODELS {
        let c = QwenVisionClient::new(key.clone(), (*m).to_string(), HttpVideoFetcher::new());
        print!("  {m:<28}");
        match c.analyze(
            &staged.reference,
            Some(dur),
            Some("这条视频画面里有什么？一句话"),
        ) {
            Ok(r) => println!(
                "✓ {:>6} video_tokens · {}",
                r.usage.video_tokens,
                r.text
                    .chars()
                    .take(38)
                    .collect::<String>()
                    .replace('\n', " ")
            ),
            Err(e) => println!("✗ {}", e.to_string().chars().take(88).collect::<String>()),
        }
    }
}

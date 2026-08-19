//! 三家平台的响应 → 同一个结构体。用夹具，不联网不花钱。
use clipknow::ingest::discovery::{parse_search_creators, parse_search_videos};
use clipknow::ingest::url::Platform;

fn fx(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}
fn o<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref()
        .map(|x| x.to_string())
        .unwrap_or_else(|| "None".into())
}
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn main() {
    let vids = [
        (
            "YouTube",
            parse_search_videos(
                Platform::YouTube,
                &fx(include_str!("../src/agent/fixtures/yt_search_videos.json")),
            ),
        ),
        (
            "TikTok",
            parse_search_videos(
                Platform::TikTok,
                &fx(include_str!("../src/agent/fixtures/tt_search_keyword.json")),
            ),
        ),
        (
            "Instagram",
            parse_search_videos(
                Platform::Instagram,
                &fx(include_str!("../src/agent/fixtures/ig_user_reels.json")),
            ),
        ),
    ];
    println!("╔═══ VideoSummary ═══════════════════════════════════════════════");
    for (name, v) in &vids {
        let v = &v[0];
        println!("║ {name}");
        println!("║   native_id      {}", v.native_id);
        println!(
            "║   title          {}",
            cut(v.title.as_deref().unwrap_or("None"), 34)
        );
        println!("║   channel_name   {}", o(&v.channel_name));
        println!("║   channel_handle {}", o(&v.channel_handle));
        println!("║   channel_id     {}", o(&v.channel_id));
        println!(
            "║   view_count     {:<12} like_count {}",
            o(&v.view_count),
            o(&v.like_count)
        );
        println!(
            "║   duration_sec   {:<12} published_at {}",
            o(&v.duration_sec),
            o(&v.published_at)
        );
        println!("║");
    }
    let crs = [
        (
            "YouTube",
            parse_search_creators(
                Platform::YouTube,
                &fx(include_str!(
                    "../src/agent/fixtures/yt_search_channels.json"
                )),
            ),
        ),
        (
            "TikTok",
            parse_search_creators(
                Platform::TikTok,
                &fx(include_str!("../src/agent/fixtures/tt_search_users.json")),
            ),
        ),
        (
            "Instagram",
            parse_search_creators(
                Platform::Instagram,
                &fx(include_str!(
                    "../src/agent/fixtures/ig_search_profiles.json"
                )),
            ),
        ),
    ];
    println!("╠═══ Creator ════════════════════════════════════════════════════");
    for (name, c) in &crs {
        let c = &c[0];
        println!("║ {name}");
        println!("║   id             {}", o(&c.id));
        println!("║   handle         {}", o(&c.handle));
        println!("║   name           {}", o(&c.name));
        println!(
            "║   follower_count {:<12} video_count {}",
            o(&c.follower_count),
            o(&c.video_count)
        );
        println!(
            "║   verified       {:<12} bio  {}",
            o(&c.verified),
            cut(c.bio.as_deref().unwrap_or("None"), 22)
        );
        println!("║");
    }
    println!("╚════════════════════════════════════════════════════════════════");
}

//! URL 解析：把用户丢进来的链接认出平台、抠出视频 ID。
//!
//! 这是整个项目里唯一「不碰网络、不碰数据库」的模块，所以最好测——
//! 给一个字符串，返回一个结果，没有任何外部依赖。先写它、先测它。
//!
//! 注意：抠出来的 ID **不是**用来调 ScrapeCreators 的（SC 的端点直接吃完整 URL），
//! 而是用来在数据库里做去重键，避免同一个视频抓两次花两份钱。

use crate::error::{ClipKnowError, Result};

/// 支持的平台。
///
/// `derive` 那一行让编译器自动生成一些常用能力：
/// Debug=可以打印，Clone/Copy=可以随便复制，PartialEq/Eq=可以用 == 比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    YouTube,
    TikTok,
    Instagram,
}

impl Platform {
    /// 存进数据库时用的字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::YouTube => "youtube",
            Platform::TikTok => "tiktok",
            Platform::Instagram => "instagram",
        }
    }

    /// 从数据库读回来时用。
    pub fn from_str(s: &str) -> Option<Platform> {
        match s {
            "youtube" => Some(Platform::YouTube),
            "tiktok" => Some(Platform::TikTok),
            "instagram" => Some(Platform::Instagram),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub platform: Platform,
    pub native_id: String,
}

/// 解析一个视频链接。
///
/// 支持的形式：
/// - YouTube: `youtube.com/watch?v=ID`、`youtu.be/ID`、`youtube.com/shorts/ID`
/// - TikTok:  `tiktok.com/@user/video/ID`、`vm.tiktok.com/ID`
/// - Instagram: `instagram.com/p/CODE`、`instagram.com/reel/CODE`
pub fn parse(raw: &str) -> Result<ParsedUrl> {
    let url = raw.trim();
    // 去掉协议头，统一成 "host/path?query" 的形式，后面好处理
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let without_www = without_scheme.strip_prefix("www.").unwrap_or(without_scheme);
    let lower = without_www.to_ascii_lowercase();

    if lower.starts_with("youtube.com/") || lower.starts_with("m.youtube.com/") {
        return parse_youtube(without_www, raw);
    }
    if lower.starts_with("youtu.be/") {
        let id = segment_after(without_www, "youtu.be/")
            .ok_or_else(|| ClipKnowError::NoVideoId(raw.to_string()))?;
        return ok(Platform::YouTube, id);
    }
    if lower.contains("tiktok.com/") {
        return parse_tiktok(without_www, raw);
    }
    if lower.starts_with("instagram.com/") || lower.starts_with("m.instagram.com/") {
        return parse_instagram(without_www, raw);
    }

    Err(ClipKnowError::UnsupportedUrl(raw.to_string()))
}

fn parse_youtube(s: &str, raw: &str) -> Result<ParsedUrl> {
    // /shorts/ID 形式
    if let Some(id) = segment_after(s, "/shorts/") {
        return ok(Platform::YouTube, id);
    }
    // /watch?v=ID 形式：从 query 里找 v 参数
    if let Some(q) = s.split_once('?').map(|(_, q)| q) {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("v=") {
                if !v.is_empty() {
                    return ok(Platform::YouTube, v.to_string());
                }
            }
        }
    }
    Err(ClipKnowError::NoVideoId(raw.to_string()))
}

fn parse_tiktok(s: &str, raw: &str) -> Result<ParsedUrl> {
    // 标准长链：tiktok.com/@handle/video/1234567890
    if let Some(id) = segment_after(s, "/video/") {
        return ok(Platform::TikTok, id);
    }
    // 短链：vm.tiktok.com/ABC123 或 vt.tiktok.com/ABC123
    // 短码不是真正的 aweme_id，但作为去重键够用（同一个短链必然指向同一个视频）
    for prefix in ["vm.tiktok.com/", "vt.tiktok.com/"] {
        if let Some(id) = segment_after(s, prefix) {
            return ok(Platform::TikTok, id);
        }
    }
    Err(ClipKnowError::NoVideoId(raw.to_string()))
}

fn parse_instagram(s: &str, raw: &str) -> Result<ParsedUrl> {
    // reels 要放在 reel 前面判断，否则 "/reel/" 会先匹配上 "/reels/xxx" 的前缀
    for prefix in ["/reels/", "/reel/", "/p/", "/tv/"] {
        if let Some(id) = segment_after(s, prefix) {
            return ok(Platform::Instagram, id);
        }
    }
    Err(ClipKnowError::NoVideoId(raw.to_string()))
}

/// 取 `marker` 后面那一段，遇到 `/`、`?`、`#` 就停。
///
/// 例：segment_after("youtube.com/shorts/abc?x=1", "/shorts/") == Some("abc")
fn segment_after(s: &str, marker: &str) -> Option<String> {
    let idx = s.to_ascii_lowercase().find(&marker.to_ascii_lowercase())?;
    let rest = &s[idx + marker.len()..];
    let end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let seg = &rest[..end];
    if seg.is_empty() { None } else { Some(seg.to_string()) }
}

/// 小工具：少写几遍 Ok(ParsedUrl { .. })
fn ok(platform: Platform, native_id: String) -> Result<ParsedUrl> {
    Ok(ParsedUrl { platform, native_id })
}

// ---------------------------------------------------------------------------
// 测试。`cargo test` 会自动跑这里的所有 #[test] 函数。
// 在 Rust 里测试和被测代码写在同一个文件是惯例，不是偷懒。
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助函数：断言解析结果
    fn assert_parses(url: &str, platform: Platform, id: &str) {
        let got = parse(url).unwrap_or_else(|e| panic!("解析 {url} 失败: {e}"));
        assert_eq!(got.platform, platform, "平台判断错了: {url}");
        assert_eq!(got.native_id, id, "视频 ID 抠错了: {url}");
    }

    #[test]
    fn youtube_watch_url() {
        assert_parses(
            "https://www.youtube.com/watch?v=UZvJzKNJ3dY",
            Platform::YouTube,
            "UZvJzKNJ3dY",
        );
    }

    #[test]
    fn youtube_watch_url_with_extra_params() {
        // 真实分享出来的链接常带 &t= 之类的参数
        assert_parses(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
            Platform::YouTube,
            "dQw4w9WgXcQ",
        );
    }

    #[test]
    fn youtube_short_link() {
        assert_parses("https://youtu.be/dQw4w9WgXcQ", Platform::YouTube, "dQw4w9WgXcQ");
    }

    #[test]
    fn youtube_shorts() {
        assert_parses(
            "https://www.youtube.com/shorts/abc123XYZ",
            Platform::YouTube,
            "abc123XYZ",
        );
    }

    #[test]
    fn tiktok_standard_url() {
        assert_parses(
            "https://www.tiktok.com/@artistcraigkirkland/video/7660954507282533662",
            Platform::TikTok,
            "7660954507282533662",
        );
    }

    #[test]
    fn tiktok_short_link() {
        assert_parses("https://vm.tiktok.com/ZMh1a2b3c/", Platform::TikTok, "ZMh1a2b3c");
    }

    #[test]
    fn instagram_post() {
        assert_parses("https://www.instagram.com/p/C1a2B3c4D5e/", Platform::Instagram, "C1a2B3c4D5e");
    }

    #[test]
    fn instagram_reel() {
        assert_parses(
            "https://www.instagram.com/reel/C9z8Y7x6W5v/",
            Platform::Instagram,
            "C9z8Y7x6W5v",
        );
    }

    #[test]
    fn instagram_reels_plural() {
        // /reels/ 复数形式也要认，且不能被 /reel/ 的分支抢先匹配错
        assert_parses(
            "https://www.instagram.com/reels/AAbbCCdd11/",
            Platform::Instagram,
            "AAbbCCdd11",
        );
    }

    #[test]
    fn accepts_url_without_scheme() {
        assert_parses("youtube.com/watch?v=abc", Platform::YouTube, "abc");
    }

    #[test]
    fn rejects_unknown_platform() {
        let err = parse("https://www.bilibili.com/video/BV1xx").unwrap_err();
        assert!(
            matches!(err, ClipKnowError::UnsupportedUrl(_)),
            "应该报 UnsupportedUrl，实际是: {err}"
        );
    }

    #[test]
    fn rejects_youtube_url_without_video_id() {
        let err = parse("https://www.youtube.com/feed/subscriptions").unwrap_err();
        assert!(
            matches!(err, ClipKnowError::NoVideoId(_)),
            "应该报 NoVideoId，实际是: {err}"
        );
    }

    #[test]
    fn platform_string_roundtrip() {
        for p in [Platform::YouTube, Platform::TikTok, Platform::Instagram] {
            assert_eq!(Platform::from_str(p.as_str()), Some(p));
        }
    }
}

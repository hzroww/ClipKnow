//! 内容层的数据结构：把三个平台各不相同的字段，统一成一套我们自己的类型。
//!
//! 这里的类型是「中立的」——不属于 ScrapeCreators，也不属于任何平台。
//! 换抓取供应商时，只有 `ingest/` 里的翻译代码要改，这些结构体不动。
//! （同样的道理稍后也用在 `LlmClient` 上：不让第三方的概念渗进业务代码。）

use crate::ingest::url::Platform;

/// 一个视频的元数据。对应数据库里的 `videos` 表。
#[derive(Debug, Clone)]
pub struct Video {
    /// 我们自己生成的 uuid v7（带时间戳，天然按时间有序）
    pub id: String,
    pub platform: Platform,
    /// 视频在平台上的原始 ID，和 platform 一起构成去重键
    pub native_id: String,
    pub url: String,
    pub title: Option<String>,
    pub author_handle: Option<String>,
    pub author_name: Option<String>,
    pub duration_sec: Option<i64>,
    /// Unix 时间戳（秒）
    pub published_at: Option<i64>,
    pub view_count: Option<i64>,
    pub like_count: Option<i64>,
    pub comment_count: Option<i64>,
    pub description: Option<String>,
    /// ★ SC 返回的原始 JSON，整个存下来。
    /// 解析时一定会漏字段（当时没想到要用封面图、或者 SC 后来加了新字段），
    /// 有它在就随时能补，不用重新花钱抓一遍。
    pub raw_json: String,
    pub fetched_at: i64,
}

/// 视频的文字稿。
#[derive(Debug, Clone)]
pub struct Transcript {
    pub video_id: String,
    pub text: String,
    /// 'sc' = ScrapeCreators 给的；以后自建语音识别时会是 'asr'
    pub source: String,
    pub lang: Option<String>,
    pub fetched_at: i64,
}

/// 一条评论。
#[derive(Debug, Clone)]
pub struct Comment {
    pub id: String,
    pub video_id: String,
    pub author: Option<String>,
    pub text: String,
    pub like_count: Option<i64>,
    pub published_at: Option<i64>,
}

/// 一次抓取的完整结果：元数据 + 可选的文字稿 + 评论。
#[derive(Debug, Clone)]
pub struct FetchedVideo {
    pub video: Video,
    pub transcript: Option<Transcript>,
    pub comments: Vec<Comment>,
}

/// 当前 Unix 时间戳（秒）。
pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 生成一个新的 uuid v7 字符串。
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 把 ISO8601 时间字符串解析成 Unix 秒。解析不了就返回 None，不报错——
/// 发布时间缺失不该让整次抓取失败。
///
/// SC 实际返回过这两种形态：
/// - `2026-03-01T14:00:18-08:00`（带时区偏移）
/// - `2025-08-17T06:41:03.513Z`（UTC，带毫秒）
pub fn parse_iso8601(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offset_format() {
        // SC 的 YouTube publishDate 实测格式
        let ts = parse_iso8601("2026-03-01T14:00:18-08:00").expect("应该能解析");
        // 2026-03-01T22:00:18Z
        assert_eq!(ts, 1772402418);
    }

    #[test]
    fn parses_utc_millis_format() {
        // SC 的评论 publishedTime 实测格式
        let ts = parse_iso8601("2025-08-17T06:41:03.513Z").expect("应该能解析");
        assert_eq!(ts, 1755412863);
    }

    #[test]
    fn returns_none_on_garbage_instead_of_failing() {
        assert_eq!(parse_iso8601("3 weeks ago"), None);
        assert_eq!(parse_iso8601(""), None);
    }

    #[test]
    fn ids_are_unique_and_time_ordered() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b, "两次生成的 id 不能相同");
        // uuid v7 前缀是时间戳，所以后生成的字典序更大
        assert!(a < b, "v7 应该按时间有序: {a} vs {b}");
    }
}

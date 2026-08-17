//! ScrapeCreators 客户端：把一个视频链接变成结构化数据。
//!
//! 三个平台返回的 JSON 结构完全不同（YouTube 是自己整理过的扁平结构，
//! TikTok 直接透传原生的 `aweme_detail`，Instagram 又是另一套），所以这里
//! 每个平台一个 normalize 函数，把它们统一成 `content::model` 里的类型。
//!
//! 取字段用的是「多候选路径」的写法而不是强类型 struct，原因有二：
//! 1. 三家字段名差异太大，写三套 struct 很啰嗦
//! 2. 平台字段随时可能改；取不到就是 None，不会让整次抓取崩掉
//! 反正原始 JSON 我们整个存进 `raw_json` 了，将来要补字段随时能补。

use serde_json::Value;

use crate::content::model::{
    now_ts, new_id, parse_iso8601, Comment, FetchedVideo, Transcript, Video,
};
use crate::error::{ClipKnowError, Result};
use crate::ingest::url::{ParsedUrl, Platform};

const BASE_URL: &str = "https://api.scrapecreators.com";
/// 抓多少条评论就够了。评论主要给模型当「观众怎么看」的证据，不需要全量。
const COMMENT_LIMIT: usize = 20;

pub struct ScrapeCreators {
    http: reqwest::blocking::Client,
    api_key: String,
}

impl ScrapeCreators {
    pub fn new(api_key: String) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("构造 HTTP 客户端失败");
        Self { http, api_key }
    }

    /// 从环境变量读 key。key 放在 ~/.zshrc 里。
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("SCRAPECREATORS_API_KEY")
            .map_err(|_| ClipKnowError::MissingEnv("SCRAPECREATORS_API_KEY"))?;
        Ok(Self::new(key))
    }

    /// 抓一个视频的全部内容：元数据 + 文字稿 + 评论。
    ///
    /// 元数据抓不到就整个失败（没有元数据这条记录没意义）；
    /// 文字稿和评论抓不到只是缺一块证据，不影响主流程。
    pub fn fetch(&self, parsed: &ParsedUrl, raw_url: &str) -> Result<FetchedVideo> {
        let detail = self.get(detail_endpoint(parsed.platform), raw_url)?;

        let video = match parsed.platform {
            Platform::YouTube => normalize_youtube(&detail, parsed, raw_url),
            Platform::TikTok => normalize_tiktok(&detail, parsed, raw_url),
            Platform::Instagram => normalize_instagram(&detail, parsed, raw_url),
        };

        // 文字稿：失败不致命，打个提示继续
        let transcript = self.fetch_transcript(parsed.platform, raw_url, &video.id);

        // 评论：同样失败不致命
        let comments = match self.get(comments_endpoint(parsed.platform), raw_url) {
            Ok(v) => extract_comments(&v, &video.id, parsed.platform),
            Err(e) => {
                eprintln!("  ! 评论抓取失败（继续）: {e}");
                Vec::new()
            }
        };

        Ok(FetchedVideo { video, transcript, comments })
    }

    /// 抓文字稿，AI 转录失败时重试一次。
    ///
    /// 为什么要重试：TikTok / Instagram 走的是 SC 自己跑的 AI 转录，实测**不稳定**——
    /// 同一个视频这次返回「Please provide the audio file...」（模型的错误回复），
    /// 下一次却能返回完整正确的文字稿。不重试的话用户会随机遇到
    /// 「这个视频没有文字稿」的假象，而实际上是有的。
    ///
    /// 代价是失败那次多花 1 个 credit。只在**确认是 AI 失败**时才重试——
    /// 真的没有人声（纯画面 + BGM）不会触发，避免白花钱。
    fn fetch_transcript(&self, platform: Platform, url: &str, video_id: &str) -> Option<Transcript> {
        for attempt in 1..=2 {
            let body = match self.get(transcript_endpoint(platform), url) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("  ! 文字稿抓取失败（继续）: {e}");
                    return None;
                }
            };

            if let Some(t) = extract_transcript(&body, video_id) {
                return Some(t);
            }

            // 提取不到。区分两种情况：AI 转录失败（值得重试）vs 真的没人声（别浪费钱）
            let is_ai_failure = raw_transcript_text(&body)
                .map(|s| looks_like_transcription_failure(&s))
                .unwrap_or(false);

            if attempt == 1 && is_ai_failure {
                eprintln!("  ! AI 转录这次失败了，重试一次 ...");
                continue;
            }
            return None;
        }
        None
    }

    /// 发一个 GET 请求，检查 SC 的 success 字段，返回解析好的 JSON。
    fn get(&self, path: &str, url_param: &str) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{BASE_URL}{path}"))
            .header("x-api-key", &self.api_key)
            .query(&[("url", url_param)])
            .send()?;

        let status = resp.status();
        let body: Value = resp.json()?;

        if !status.is_success() {
            return Err(ClipKnowError::Fetch {
                platform: path.to_string(),
                message: format!("HTTP {status}: {}", truncate(&body.to_string(), 200)),
            });
        }
        // SC 即使 HTTP 200 也可能返回 success:false
        if body.get("success").and_then(Value::as_bool) == Some(false) {
            let msg = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("SC 返回 success:false");
            return Err(ClipKnowError::Fetch {
                platform: path.to_string(),
                message: msg.to_string(),
            });
        }
        Ok(body)
    }
}

// ---------------------------------------------------------------------------
// 端点映射
// ---------------------------------------------------------------------------

fn detail_endpoint(p: Platform) -> &'static str {
    match p {
        Platform::YouTube => "/v1/youtube/video",
        Platform::TikTok => "/v2/tiktok/video",
        Platform::Instagram => "/v1/instagram/post",
    }
}

fn transcript_endpoint(p: Platform) -> &'static str {
    match p {
        Platform::YouTube => "/v1/youtube/video/transcript",
        Platform::TikTok => "/v1/tiktok/video/transcript",
        Platform::Instagram => "/v2/instagram/media/transcript",
    }
}

fn comments_endpoint(p: Platform) -> &'static str {
    match p {
        Platform::YouTube => "/v1/youtube/video/comments",
        Platform::TikTok => "/v1/tiktok/video/comments",
        Platform::Instagram => "/v2/instagram/post/comments",
    }
}

// ---------------------------------------------------------------------------
// 各平台的字段翻译
// ---------------------------------------------------------------------------

/// YouTube：SC 已经整理成扁平结构，字段名见 `/v1/youtube/video` 实测结果。
fn normalize_youtube(v: &Value, parsed: &ParsedUrl, raw_url: &str) -> Video {
    Video {
        id: new_id(),
        platform: parsed.platform,
        native_id: parsed.native_id.clone(),
        url: raw_url.to_string(),
        title: pick_str(v, &["title"]),
        author_handle: pick_str(v, &["channel.handle"]),
        author_name: pick_str(v, &["channel.title"]),
        // SC 给的是毫秒，我们统一存秒
        duration_sec: pick_i64(v, &["durationMs"]).map(|ms| ms / 1000),
        published_at: pick_str(v, &["publishDate"]).and_then(|s| parse_iso8601(&s)),
        view_count: pick_i64(v, &["viewCountInt"]),
        like_count: pick_i64(v, &["likeCountInt"]),
        comment_count: pick_i64(v, &["commentCountInt"]),
        description: pick_str(v, &["description"]),
        raw_json: v.to_string(),
        fetched_at: now_ts(),
    }
}

/// TikTok：SC 透传 TikTok 原生的 `aweme_detail`，字段名是平台自己的风格。
fn normalize_tiktok(v: &Value, parsed: &ParsedUrl, raw_url: &str) -> Video {
    let a = v.get("aweme_detail").unwrap_or(v);
    Video {
        id: new_id(),
        platform: parsed.platform,
        native_id: parsed.native_id.clone(),
        url: raw_url.to_string(),
        // TikTok 没有独立标题，desc 就是文案
        title: pick_str(a, &["desc"]),
        author_handle: pick_str(a, &["author.unique_id"]),
        author_name: pick_str(a, &["author.nickname"]),
        duration_sec: pick_i64(a, &["video.duration"]).map(|ms| ms / 1000),
        // create_time 已经是 Unix 秒，不用解析
        published_at: pick_i64(a, &["create_time"]),
        view_count: pick_i64(a, &["statistics.play_count"]),
        like_count: pick_i64(a, &["statistics.digg_count"]),
        comment_count: pick_i64(a, &["statistics.comment_count"]),
        description: pick_str(a, &["desc"]),
        raw_json: v.to_string(),
        fetched_at: now_ts(),
    }
}

/// Instagram：已用真实 reel 实测通过（标题/作者/时长/播放量/点赞全部正确提取）。
/// 多候选路径保留着——IG 的返回结构在不同内容类型间会变，多留几个候选更耐改。
fn normalize_instagram(v: &Value, parsed: &ParsedUrl, raw_url: &str) -> Video {
    // SC 可能把内容包在 data / items[0] / 直接平铺，都试一遍
    let n = first_present(v, &["data.xdt_shortcode_media", "data", "items.0", "post"]).unwrap_or(v);
    Video {
        id: new_id(),
        platform: parsed.platform,
        native_id: parsed.native_id.clone(),
        url: raw_url.to_string(),
        title: pick_str(n, &["caption.text", "caption", "edge_media_to_caption.edges.0.node.text"]),
        author_handle: pick_str(n, &["owner.username", "user.username"]),
        author_name: pick_str(n, &["owner.full_name", "user.full_name"]),
        duration_sec: pick_f64(n, &["video_duration"]).map(|d| d as i64),
        published_at: pick_i64(n, &["taken_at_timestamp", "taken_at"])
            .or_else(|| pick_str(n, &["taken_at"]).and_then(|s| parse_iso8601(&s))),
        view_count: pick_i64(n, &["video_play_count", "play_count", "video_view_count"]),
        like_count: pick_i64(n, &["like_count", "edge_media_preview_like.count"]),
        comment_count: pick_i64(n, &["comment_count", "edge_media_to_comment.count"]),
        description: pick_str(n, &["caption.text", "caption"]),
        raw_json: v.to_string(),
        fetched_at: now_ts(),
    }
}

/// 文字稿。三个平台的返回格式**完全不同**，都是实测出来的：
///
/// | 平台      | 字段                   | 格式                    |
/// |-----------|------------------------|-------------------------|
/// | YouTube   | `transcript_only_text` | 纯文本                  |
/// | TikTok    | `transcript`           | WEBVTT 字幕（带时间戳） |
/// | Instagram | `transcripts[].text`   | 纯文本（注意是复数）    |
///
/// 另外 IG/TikTok 走的是 SC 自己跑的 AI 转录，失败时不会报错，
/// 而是返回一句模型的错误回复（比如「Please provide the audio file...」）。
/// 这种要识别出来当作「没有转录」，否则会拿一句废话去喂模型。
fn extract_transcript(v: &Value, video_id: &str) -> Option<Transcript> {
    let raw = raw_transcript_text(v)?;

    // WEBVTT 要把时间戳行剥掉，只留台词
    let text = if is_webvtt(&raw) { strip_webvtt(&raw) } else { raw };
    let text = text.trim().to_string();

    if text.is_empty() || looks_like_transcription_failure(&text) {
        return None;
    }

    Some(Transcript {
        video_id: video_id.to_string(),
        text,
        source: "sc".to_string(),
        lang: pick_str(v, &["language"]),
        fetched_at: now_ts(),
    })
}

/// 从响应里把文字稿原文取出来（不做任何过滤）。
/// 三个平台字段名各不相同，都是实测出来的。
fn raw_transcript_text(v: &Value) -> Option<String> {
    pick_str(v, &["transcript_only_text", "text", "transcript_text"])
        // Instagram：transcripts 是复数，取第一条
        .or_else(|| pick_str(v, &["transcripts.0.text"]))
        // TikTok：transcript 直接是一个 WEBVTT 字符串
        .or_else(|| pick_str(v, &["transcript"]))
        // YouTube 备用：transcript 是 [{text, startMs}] 数组
        .or_else(|| join_segments(v))
}

fn is_webvtt(s: &str) -> bool {
    s.trim_start().starts_with("WEBVTT")
}

/// 把 WEBVTT 字幕剥成纯文本：丢掉头部标记、时间戳行、序号行和空行。
fn strip_webvtt(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && *l != "WEBVTT"
                && !l.contains("-->")
                // 纯数字的是字幕序号
                && !l.chars().all(|c| c.is_ascii_digit())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 识别 AI 转录失败时吐出来的模型错误回复。
///
/// 只匹配很明确的几种说法，并且限制长度——真实的转录不会这么短又
/// 恰好长这样，这样能避免误杀正常内容。
fn looks_like_transcription_failure(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    // 超过这个长度基本就是真内容了，不再怀疑
    if t.chars().count() > 200 {
        return false;
    }
    const MARKERS: &[&str] = &[
        "please provide the audio",
        "please provide the video",
        "would like me to transcribe",
        "i cannot transcribe",
        "i'm unable to transcribe",
        "i am unable to transcribe",
        "no audio content",
        "no speech detected",
    ];
    MARKERS.iter().any(|m| t.contains(m))
}

/// 从 `transcript: [{text, startMs, ...}]` 拼出纯文本。
fn join_segments(v: &Value) -> Option<String> {
    let arr = v.get("transcript")?.as_array()?;
    let joined = arr
        .iter()
        .filter_map(|seg| seg.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    if joined.trim().is_empty() { None } else { Some(joined) }
}

fn extract_comments(v: &Value, video_id: &str, platform: Platform) -> Vec<Comment> {
    let arr = match v
        .get("comments")
        .and_then(Value::as_array)
        .or_else(|| v.get("data").and_then(Value::as_array))
    {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter()
        .take(COMMENT_LIMIT)
        .filter_map(|c| {
            // 各平台评论正文的字段名不同
            let text = pick_str(c, &["content", "text", "comment"])?;
            if text.trim().is_empty() {
                return None;
            }
            Some(Comment {
                id: pick_str(c, &["id", "cid"]).unwrap_or_else(new_id),
                video_id: video_id.to_string(),
                author: pick_str(c, &["author.name", "user.nickname", "user.username", "owner.username"]),
                text,
                like_count: pick_i64(c, &["engagement.likes", "digg_count", "like_count"]),
                published_at: pick_i64(c, &["create_time"])
                    .or_else(|| pick_str(c, &["publishedTime"]).and_then(|s| parse_iso8601(&s))),
            })
        })
        .inspect(|_| {
            let _ = platform; // platform 目前只用于将来可能的差异化处理
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 取字段的小工具：支持 "a.b.c" 和 "a.0.b" 这样的点号路径
// ---------------------------------------------------------------------------

/// 按点号路径深入取值。`items.0.name` 会依次取 items → 第 0 个元素 → name。
fn dig<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = match part.parse::<usize>() {
            Ok(idx) => cur.get(idx)?,   // 纯数字当数组下标
            Err(_) => cur.get(part)?,   // 否则当对象的 key
        };
    }
    Some(cur)
}

/// 依次尝试多个路径，返回第一个存在且非 null 的节点。
fn first_present<'a>(v: &'a Value, paths: &[&str]) -> Option<&'a Value> {
    paths.iter().find_map(|p| dig(v, p).filter(|x| !x.is_null()))
}

fn pick_str(v: &Value, paths: &[&str]) -> Option<String> {
    let node = first_present(v, paths)?;
    match node {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn pick_i64(v: &Value, paths: &[&str]) -> Option<i64> {
    let node = first_present(v, paths)?;
    node.as_i64()
        .or_else(|| node.as_f64().map(|f| f as i64))
        // 有些平台把数字塞在字符串里
        .or_else(|| node.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn pick_f64(v: &Value, paths: &[&str]) -> Option<f64> {
    first_present(v, paths)?.as_f64()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ---------------------------------------------------------------------------
// 测试：全部用实测抓到的真实响应片段，不碰网络
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(platform: Platform, id: &str) -> ParsedUrl {
        ParsedUrl { platform, native_id: id.to_string() }
    }

    #[test]
    fn dig_walks_nested_objects_and_arrays() {
        let v = json!({"a": {"b": [{"c": 42}]}});
        assert_eq!(dig(&v, "a.b.0.c").and_then(Value::as_i64), Some(42));
        assert!(dig(&v, "a.b.9.c").is_none(), "越界应返回 None 而不是 panic");
        assert!(dig(&v, "nope").is_none());
    }

    #[test]
    fn pick_i64_handles_numbers_in_strings() {
        let v = json!({"a": 5, "b": "7", "c": 1.9, "d": null});
        assert_eq!(pick_i64(&v, &["a"]), Some(5));
        assert_eq!(pick_i64(&v, &["b"]), Some(7));
        assert_eq!(pick_i64(&v, &["c"]), Some(1));
        assert_eq!(pick_i64(&v, &["d"]), None, "null 应视为缺失");
    }

    #[test]
    fn first_present_falls_through_to_later_candidates() {
        let v = json!({"second": "hit"});
        assert_eq!(pick_str(&v, &["first", "second"]), Some("hit".into()));
    }

    #[test]
    fn normalizes_real_youtube_response() {
        // 字段取自 /v1/youtube/video?url=...UZvJzKNJ3dY 的真实响应
        let v = json!({
            "success": true,
            "id": "UZvJzKNJ3dY",
            "title": "Braun Whole Range 2026 – National Product Review",
            "description": "When life gets busy...",
            "likeCountInt": 1,
            "viewCountInt": 231,
            "commentCountInt": null,
            "publishDate": "2026-03-01T14:00:18-08:00",
            "durationMs": 57000,
            "channel": {
                "id": "UCpezCSL_alOva5WKM1MXH2Q",
                "handle": "nationalproductreview2267",
                "title": "National Product Review"
            }
        });
        let got = normalize_youtube(&v, &parsed(Platform::YouTube, "UZvJzKNJ3dY"), "https://x");

        assert_eq!(got.title.as_deref(), Some("Braun Whole Range 2026 – National Product Review"));
        assert_eq!(got.author_handle.as_deref(), Some("nationalproductreview2267"));
        assert_eq!(got.author_name.as_deref(), Some("National Product Review"));
        assert_eq!(got.duration_sec, Some(57), "毫秒要换算成秒");
        assert_eq!(got.view_count, Some(231));
        assert_eq!(got.like_count, Some(1));
        assert_eq!(got.comment_count, None, "SC 实测这个字段可能是 null");
        assert_eq!(got.published_at, Some(1772402418));
        assert!(!got.raw_json.is_empty(), "原始 JSON 必须存下来");
    }

    #[test]
    fn normalizes_real_tiktok_response() {
        // 字段取自 /v2/tiktok/video?url=...7660954507282533662 的真实响应
        let v = json!({
            "success": true,
            "aweme_detail": {
                "aweme_id": "7660954507282533662",
                "desc": "Mek me tell yuh dis",
                "create_time": 1783704990,
                "author": { "unique_id": "artistcraigkirkland", "nickname": "Amaziyah The Great Music" },
                "statistics": { "play_count": 77, "digg_count": 9, "comment_count": 0 },
                "video": { "duration": 25067 }
            }
        });
        let got = normalize_tiktok(&v, &parsed(Platform::TikTok, "7660954507282533662"), "https://x");

        assert_eq!(got.title.as_deref(), Some("Mek me tell yuh dis"));
        assert_eq!(got.author_handle.as_deref(), Some("artistcraigkirkland"));
        assert_eq!(got.author_name.as_deref(), Some("Amaziyah The Great Music"));
        assert_eq!(got.duration_sec, Some(25), "25067ms → 25s");
        assert_eq!(got.published_at, Some(1783704990), "create_time 本来就是秒，不该再解析");
        assert_eq!(got.view_count, Some(77));
        assert_eq!(got.like_count, Some(9), "TikTok 的点赞是 digg_count");
    }

    #[test]
    fn extracts_transcript_from_only_text_field() {
        let v = json!({
            "transcript_only_text": "  We're no strangers to love  ",
            "language": "English"
        });
        let t = extract_transcript(&v, "vid-1").expect("应该有文字稿");
        assert_eq!(t.text, "We're no strangers to love", "首尾空白要去掉");
        assert_eq!(t.lang.as_deref(), Some("English"));
        assert_eq!(t.source, "sc");
    }

    #[test]
    fn falls_back_to_joining_transcript_segments() {
        let v = json!({ "transcript": [{"text": "hello"}, {"text": "world"}] });
        let t = extract_transcript(&v, "vid-1").expect("应该能从分段拼出来");
        assert_eq!(t.text, "hello world");
    }

    #[test]
    fn empty_transcript_is_none_not_empty_string() {
        // 纯画面+BGM 的视频会走到这里，必须返回 None，让上层老实说「没有语音内容」
        assert!(extract_transcript(&json!({"transcript_only_text": "   "}), "v").is_none());
        assert!(extract_transcript(&json!({}), "v").is_none());
    }

    #[test]
    fn parses_tiktok_webvtt_transcript() {
        // TikTok 实测返回的就是这个格式：一整个 WEBVTT 字符串，不是数组
        let v = json!({
            "transcript": "WEBVTT\n\n\n00:00:00.000 --> 00:00:01.460\nWell, if you know Tom,\n\n00:00:01.461 --> 00:00:04.181\nyou must know Jerry."
        });
        let t = extract_transcript(&v, "vid-1").expect("WEBVTT 也要能解析出文字稿");
        assert_eq!(t.text, "Well, if you know Tom, you must know Jerry.");
        assert!(!t.text.contains("-->"), "时间戳必须被剥掉");
        assert!(!t.text.contains("WEBVTT"), "头部标记必须被剥掉");
    }

    #[test]
    fn strips_webvtt_cue_numbers() {
        let vtt = "WEBVTT\n\n1\n00:00:00.000 --> 00:00:01.000\n第一句\n\n2\n00:00:01.000 --> 00:00:02.000\n第二句";
        assert_eq!(strip_webvtt(vtt), "第一句 第二句", "序号行也要丢掉");
    }

    #[test]
    fn parses_instagram_plural_transcripts_field() {
        // Instagram 实测：字段名是复数 transcripts，内容在 [0].text
        let v = json!({
            "transcripts": [{"id": "1", "shortcode": "ABC", "text": "先把锅烧热，然后下油。"}]
        });
        let t = extract_transcript(&v, "vid-1").expect("IG 的复数字段也要认");
        assert_eq!(t.text, "先把锅烧热，然后下油。");
    }

    #[test]
    fn rejects_ai_transcription_failure_message() {
        // IG 实测遇到的真实垃圾输出：SC 的 AI 转录失败时返回模型的错误回复。
        // 不识别的话，模型会拿着这句废话去分析视频。
        let v = json!({
            "transcripts": [{
                "text": "Please provide the audio or video file you would like me to transcribe."
            }]
        });
        assert!(
            extract_transcript(&v, "vid-1").is_none(),
            "AI 转录失败的回复必须当作「没有转录」"
        );
    }

    #[test]
    fn failure_detection_does_not_eat_real_content() {
        // 防误杀：真实转录里可能碰巧提到「audio」之类的词
        let v = json!({
            "transcript_only_text": "今天教大家怎么给视频配音。首先打开软件，导入 audio 文件，\
调整音量。然后我们来看第二步，这一步很关键，很多人都做错了。记得把降噪打开，\
不然背景里的杂音会很明显。最后导出的时候选择高质量。"
        });
        assert!(
            extract_transcript(&v, "vid-1").is_some(),
            "正常内容不能被误判成失败"
        );
    }

    #[test]
    fn long_text_is_never_treated_as_failure() {
        // 超过 200 字的内容一律当真内容，即使开头像失败提示
        let long = format!("please provide the audio{}", "内容".repeat(200));
        assert!(!looks_like_transcription_failure(&long));
    }

    #[test]
    fn extracts_real_youtube_comments() {
        // 字段取自 /v1/youtube/video/comments 的真实响应
        let v = json!({
            "comments": [{
                "id": "Ugzge340dBgB75hWBm54AaABAg",
                "content": "can confirm: he never gave us up",
                "publishedTime": "2025-08-17T06:41:03.513Z",
                "author": { "name": "@YouTube" },
                "engagement": { "likes": 301000, "replies": 962 }
            }]
        });
        let cs = extract_comments(&v, "vid-1", Platform::YouTube);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].text, "can confirm: he never gave us up");
        assert_eq!(cs[0].author.as_deref(), Some("@YouTube"));
        assert_eq!(cs[0].like_count, Some(301000));
        assert_eq!(cs[0].published_at, Some(1755412863));
    }

    #[test]
    fn comments_are_capped_and_blanks_dropped() {
        let mut list: Vec<Value> = (0..50).map(|i| json!({"content": format!("c{i}")})).collect();
        list.push(json!({"content": "   "}));
        let v = json!({ "comments": list });
        let cs = extract_comments(&v, "vid-1", Platform::YouTube);
        assert_eq!(cs.len(), COMMENT_LIMIT, "应该截断到 {COMMENT_LIMIT} 条");
    }

    #[test]
    fn missing_comments_field_yields_empty_not_panic() {
        assert!(extract_comments(&json!({}), "v", Platform::YouTube).is_empty());
    }
}

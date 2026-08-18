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
    pub fetched_at: i64,
    // 注意这里没有 raw_json。原始响应统一放在 Artifact 里——
    // 三个端点各存一份，而不是只留详情那一份。
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

/// 一次端点调用的结局。
///
/// 关键是区分 `Unavailable` 和 `Failed`：
/// - `Unavailable`：调用成功，内容**确实没有**（纯画面视频没人声、视频没评论）。
///   这是确定的信息，应该覆盖掉旧数据。
/// - `Failed`：调用**失败了**，我们不知道有没有。这时绝不能覆盖旧数据——
///   否则 `--refresh` 遇到一次网络抖动，就把上次抓好的评论清空了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStatus {
    Ok,
    Unavailable,
    Failed,
}

impl FetchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FetchStatus::Ok => "ok",
            FetchStatus::Unavailable => "unavailable",
            FetchStatus::Failed => "failed",
        }
    }

    /// 这个结局是否足以确定内容状态、可以覆盖旧数据。
    pub fn is_conclusive(&self) -> bool {
        matches!(self, FetchStatus::Ok | FetchStatus::Unavailable)
    }

    pub fn from_db(s: &str) -> FetchStatus {
        match s {
            "ok" => FetchStatus::Ok,
            "unavailable" => FetchStatus::Unavailable,
            // 认不出来就当失败：保守，不会导致误删旧数据
            _ => FetchStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Detail,
    Transcript,
    Comments,
}

impl ArtifactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Detail => "detail",
            ArtifactKind::Transcript => "transcript",
            ArtifactKind::Comments => "comments",
        }
    }

    pub fn from_db(s: &str) -> Option<ArtifactKind> {
        match s {
            "detail" => Some(ArtifactKind::Detail),
            "transcript" => Some(ArtifactKind::Transcript),
            "comments" => Some(ArtifactKind::Comments),
            _ => None,
        }
    }

    /// 给人看的名字，`show --raw` 里用。
    pub fn label(&self) -> &'static str {
        match self {
            ArtifactKind::Detail => "视频详情",
            ArtifactKind::Transcript => "文字稿",
            ArtifactKind::Comments => "评论",
        }
    }
}

/// 一次端点调用的完整记录：结局 + 原始响应。
#[derive(Debug, Clone)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub status: FetchStatus,
    /// 原始响应，原样保存。解析漏了字段以后能从这里补，不用重新花钱抓。
    pub raw_json: Option<String>,
    pub error: Option<String>,
    pub fetched_at: i64,
}

impl Artifact {
    pub fn ok(kind: ArtifactKind, raw: String) -> Self {
        Artifact {
            kind,
            status: FetchStatus::Ok,
            raw_json: Some(raw),
            error: None,
            fetched_at: now_ts(),
        }
    }
    pub fn unavailable(kind: ArtifactKind, raw: String) -> Self {
        Artifact {
            kind,
            status: FetchStatus::Unavailable,
            raw_json: Some(raw),
            error: None,
            fetched_at: now_ts(),
        }
    }
    pub fn failed(kind: ArtifactKind, err: String) -> Self {
        Artifact {
            kind,
            status: FetchStatus::Failed,
            raw_json: None,
            error: Some(err),
            fetched_at: now_ts(),
        }
    }
}

/// 一次抓取的完整结果：元数据 + 可选的文字稿 + 评论 + 每个端点的结局。
#[derive(Debug, Clone)]
pub struct FetchedVideo {
    pub video: Video,
    pub transcript: Option<Transcript>,
    pub comments: Vec<Comment>,
    /// 三个端点各一条。写库时用它决定「能不能覆盖旧数据」。
    pub artifacts: Vec<Artifact>,
}

impl FetchedVideo {
    pub fn status_of(&self, kind: ArtifactKind) -> FetchStatus {
        self.artifacts
            .iter()
            .find(|a| a.kind == kind)
            .map(|a| a.status)
            // 没有记录就当失败处理：宁可保留旧数据，也不要误删
            .unwrap_or(FetchStatus::Failed)
    }
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

/// 搜索/列表结果里的一条视频。比 `Video` 轻——没有文字稿、没有评论，
/// 只有挑选候选时需要的信息。
///
/// 这是**给模型看的**类型：字段少是刻意的，多一个字段就是每轮多一份 token。
/// 原始响应在 `tool_calls` 表里，漏了随时能补。
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSummary {
    pub platform: Platform,
    pub native_id: String,
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub channel_name: Option<String>,
    /// 用于展示和调下一个 API。**不能拿来做比较或去重**——
    /// 跨端点格式不一致（`laogao` vs `@hafu`）、中文是 URL 编码、用户还能改。
    pub channel_handle: Option<String>,
    /// 稳定 ID，去重就用它。YouTube 是 `UC...`，TikTok 是数字串，IG 是 pk。
    pub channel_id: Option<String>,
    pub view_count: Option<i64>,
    pub like_count: Option<i64>,
    pub comment_count: Option<i64>,
    pub duration_sec: Option<i64>,
    /// Unix 秒。YouTube 只能用 publishDate 算，见 discovery.rs 的注释。
    pub published_at: Option<i64>,
}

/// 一个博主。搜索结果和主页详情共用这一个类型——虽然两个端点的字段名
/// 完全不同（见 discovery.rs 的注释），但对模型来说它们是同一个概念。
#[derive(Debug, Clone, PartialEq)]
pub struct Creator {
    pub platform: Platform,
    /// 平台的稳定 ID，**去重只能用它**。
    /// YouTube 是 `UC...`，TikTok 是数字 uid，Instagram 是数字 id。
    pub id: Option<String>,
    /// 展示 + 调下一个 API 用。已统一剥掉开头的 `@`。
    /// 中文 handle 是 URL 编码串（实测 SC 编码/解码两种都认，原样透传即可）。
    pub handle: Option<String>,
    pub name: Option<String>,
    pub bio: Option<String>,
    pub follower_count: Option<i64>,
    pub video_count: Option<i64>,
    pub verified: Option<bool>,
}

// ---------------------------------------------------------------------------
// 会话存储的中立类型（第二版）
// ---------------------------------------------------------------------------

/// 一条历史条目的类型。
///
/// 类型名写全，不另设「方向」字段。
///
/// 有些实现把 user 和 assistant 消息都叫 `message`，再用一个 direction
/// 字段区分谁说的——那是为了兼容 OpenAI Responses API 的格式。这里没有
/// 这个包袱，把四种类型直接写清楚，一个字段就够，查询时也不用两个条件拼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    FunctionCall,
    FunctionCallOutput,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::UserMessage => "user_message",
            ItemKind::AssistantMessage => "assistant_message",
            ItemKind::FunctionCall => "function_call",
            ItemKind::FunctionCallOutput => "function_call_output",
        }
    }

    pub fn from_db(s: &str) -> Option<ItemKind> {
        Some(match s {
            "user_message" => ItemKind::UserMessage,
            "assistant_message" => ItemKind::AssistantMessage,
            "function_call" => ItemKind::FunctionCall,
            "function_call_output" => ItemKind::FunctionCallOutput,
            _ => return None,
        })
    }
}

/// 一次提问的终态。对应状态机的 Done / Failed。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    Done,
    Failed(String),
}

impl TurnStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnStatus::Done => "done",
            TurnStatus::Failed(_) => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub id: String,
    pub seq: i64,
    pub model: String,
    pub status: TurnStatus,
    pub created_at: i64,
}

/// 历史里的一条。
///
/// `payload` 存的是**当时实际发给模型的那段文本**，不是结构化数据——
/// 大模型 API 无状态，每轮要把完整历史重发；存结构化的话重建时得重新渲染，
/// 而渲染代码一改，模型看到的「自己上一轮读过的材料」就悄悄变了样。
///
/// `raw_json` 只有 `FunctionCallOutput` 有，而且**重建历史时不加载**。
#[derive(Debug, Clone)]
pub struct Item {
    pub idx: i64,
    pub kind: ItemKind,
    pub iteration: Option<i64>,
    pub call_id: Option<String>,
    pub payload: serde_json::Value,
    pub raw_json: Option<String>,
}

impl Item {
    pub fn user_message(idx: i64, text: &str) -> Item {
        Item {
            idx,
            kind: ItemKind::UserMessage,
            iteration: None,
            call_id: None,
            payload: serde_json::json!({ "text": text }),
            raw_json: None,
        }
    }

    pub fn assistant_message(idx: i64, iteration: i64, text: &str) -> Item {
        Item {
            idx,
            kind: ItemKind::AssistantMessage,
            iteration: Some(iteration),
            call_id: None,
            payload: serde_json::json!({ "text": text }),
            raw_json: None,
        }
    }

    pub fn function_call(
        idx: i64,
        iteration: i64,
        call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Item {
        Item {
            idx,
            kind: ItemKind::FunctionCall,
            iteration: Some(iteration),
            call_id: Some(call_id.into()),
            payload: serde_json::json!({ "name": name, "args": args }),
            raw_json: None,
        }
    }

    pub fn function_call_output(
        idx: i64,
        iteration: i64,
        call_id: &str,
        content: &str,
        is_error: bool,
        raw_json: Option<String>,
    ) -> Item {
        Item {
            idx,
            kind: ItemKind::FunctionCallOutput,
            iteration: Some(iteration),
            call_id: Some(call_id.into()),
            payload: serde_json::json!({ "content": content, "is_error": is_error }),
            raw_json,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub created_at: i64,
}

//! 发现类端点（搜索、博主主页、博主视频列表）的响应 → 中立类型。
//!
//! 和 `scrapecreators.rs` 一样是「脏活隔离区」：三家平台的字段名、嵌套深度、
//! ID 命名全不一样，全部差异关在这个文件里。
//!
//! **为什么必须精简而不是把原始 JSON 丢给模型**（2026-08-18 实测）：
//!   - 一条 TikTok 搜索结果 84.4 KB，`aweme_info` 有 166 个字段
//!   - 一次 TikTok 关键词搜索的完整响应 2.4 MB
//!   - 一次 YouTube 搜索 39 KB，精简后 7.4 KB
//!
//! DeepSeek 的上下文是 64K —— 原样塞进去，一次工具调用就爆了。
//!
//! 原始响应另存进 `tool_calls` 表，解析漏了随时能补，不用重新花钱抓。

use serde_json::Value;

use crate::content::model::FetchedVideo;
use crate::content::model::{Creator, VideoSummary, parse_iso8601};
use crate::error::Result;
use crate::ingest::scrapecreators::{dig, first_present, pick_f64, pick_i64, pick_str, truncate};
use crate::ingest::url::{ParsedUrl, Platform};

/// 简介截断长度。它是判断「这人是不是真做这个」的关键信息之一，
/// 截太狠会误判；不截则一次搜索能到 6000+ token。这个数字要跑通后再调。
pub const DESC_LIMIT_CHARS: usize = 120;

/// 搜索结果里装视频列表的字段名，三家各不相同。
fn video_list_key(p: Platform) -> &'static [&'static str] {
    match p {
        Platform::YouTube => &["videos"],
        Platform::TikTok => &["search_item_list", "aweme_list"],
        Platform::Instagram => &["reels", "items"],
    }
}

pub fn parse_search_videos(p: Platform, body: &Value) -> Vec<VideoSummary> {
    let Some(items) = video_list_key(p)
        .iter()
        .find_map(|k| body.get(k).and_then(Value::as_array))
    else {
        return Vec::new();
    };
    items.iter().filter_map(|it| to_summary(p, it)).collect()
}

fn to_summary(p: Platform, it: &Value) -> Option<VideoSummary> {
    match p {
        Platform::YouTube => youtube_summary(it),
        Platform::TikTok => tiktok_summary(it),
        Platform::Instagram => instagram_summary(it),
    }
}

fn tiktok_summary(it: &Value) -> Option<VideoSummary> {
    // 两个端点两种外壳：搜索是 search_item_list[].aweme_info，
    // 博主视频列表是 aweme_list[] 直接平铺。
    let a = it.get("aweme_info").unwrap_or(it);
    let native_id = pick_str(a, &["aweme_id"])?;
    let handle = pick_str(a, &["author.unique_id"]);
    Some(VideoSummary {
        platform: Platform::TikTok,
        url: pick_str(a, &["share_url"]).unwrap_or_else(|| {
            let h = handle.as_deref().unwrap_or("_");
            format!("https://www.tiktok.com/@{h}/video/{native_id}")
        }),
        native_id,
        // TikTok 没有独立标题，desc 就是文案
        title: pick_str(a, &["desc"]),
        description: pick_str(a, &["desc"]).map(|d| truncate(&d, DESC_LIMIT_CHARS)),
        channel_name: pick_str(a, &["author.nickname"]),
        channel_handle: handle,
        // ★ 去重用 uid（数字串），不用 unique_id —— 后者是 handle，用户能改
        channel_id: pick_str(a, &["author.uid"]),
        view_count: pick_i64(a, &["statistics.play_count"]),
        // ★ TikTok 管点赞叫 digg_count
        like_count: pick_i64(a, &["statistics.digg_count"]),
        comment_count: pick_i64(a, &["statistics.comment_count"]),
        // ★ duration 是毫秒。当成秒会算出 5.7 天。
        duration_sec: pick_i64(a, &["video.duration"]).map(|ms| ms / 1000),
        // create_time 已经是 Unix 秒，不用解析
        published_at: pick_i64(a, &["create_time"]),
    })
}

fn instagram_summary(it: &Value) -> Option<VideoSummary> {
    let m = it.get("media").unwrap_or(it);
    // ★ 用 code 不用 pk：链接里出现的是 code（DWe9Hq_EgrR），
    //   这样才能和第一版 url.rs 从链接解出的 ID 对上。
    let native_id = pick_str(m, &["code", "shortcode"])?;
    Some(VideoSummary {
        platform: Platform::Instagram,
        url: format!("https://www.instagram.com/reel/{native_id}/"),
        native_id,
        title: pick_str(m, &["caption.text"]).map(|t| truncate(&t, DESC_LIMIT_CHARS)),
        description: pick_str(m, &["caption.text"]).map(|t| truncate(&t, DESC_LIMIT_CHARS)),
        channel_name: pick_str(m, &["user.full_name", "owner.full_name"]),
        channel_handle: pick_str(m, &["user.username", "owner.username"]),
        channel_id: pick_str(m, &["user.pk", "user.id", "owner.id"]),
        view_count: pick_i64(m, &["play_count", "video_play_count", "view_count"]),
        like_count: pick_i64(m, &["like_count"]),
        comment_count: pick_i64(m, &["comment_count"]),
        // ★ video_duration 是浮点秒（65.33899688720703），当整数取会拿不到
        duration_sec: pick_f64(m, &["video_duration"]).map(|d| d as i64),
        published_at: pick_i64(m, &["taken_at", "taken_at_timestamp"]),
    })
}

fn youtube_summary(v: &Value) -> Option<VideoSummary> {
    let native_id = pick_str(v, &["id"])?;
    Some(VideoSummary {
        platform: Platform::YouTube,
        url: pick_str(v, &["url"])
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={native_id}")),
        native_id,
        title: pick_str(v, &["title"]),
        description: pick_str(v, &["description"]).map(|d| truncate(&d, DESC_LIMIT_CHARS)),
        channel_name: pick_str(v, &["channel.title", "channel.name"]),
        channel_handle: pick_str(v, &["channel.handle"]),
        channel_id: pick_str(v, &["channel.id", "channelId"]),
        view_count: pick_i64(v, &["viewCountInt"]),
        like_count: pick_i64(v, &["likeCountInt"]),
        comment_count: pick_i64(v, &["commentCountInt"]),
        duration_sec: pick_i64(v, &["lengthSeconds"]),
        // ★ 只用 publishDate。publishedTime 是从「1 month ago」这种模糊文本
        //   反推的，实测和 publishDate 能差 19 天，而且会是 null。
        published_at: dig(v, "publishDate")
            .and_then(Value::as_str)
            .and_then(parse_iso8601),
    })
}

/// 搜索结果里装博主列表的字段名，三家各不相同。
fn creator_list_key(p: Platform) -> &'static [&'static str] {
    match p {
        Platform::YouTube => &["channels"],
        Platform::TikTok => &["user_list"],
        Platform::Instagram => &["profiles", "users"],
    }
}

pub fn parse_search_creators(p: Platform, body: &Value) -> Vec<Creator> {
    let Some(items) = creator_list_key(p)
        .iter()
        .find_map(|k| body.get(k).and_then(Value::as_array))
    else {
        return Vec::new();
    };
    items.iter().filter_map(|it| to_creator(p, it)).collect()
}

/// 博主主页详情。注意这几个端点的字段名和搜索结果里**完全不一样**，
/// 所以下面每个字段都给了多个候选路径，两个端点共用一套解析。
pub fn parse_creator(p: Platform, body: &Value) -> Option<Creator> {
    to_creator(p, body)
}

fn to_creator(p: Platform, v: &Value) -> Option<Creator> {
    // 各家把博主对象藏在不同深度：
    //   YouTube 搜索/详情都平铺；TikTok 搜索在 user_info、详情在 user；
    //   Instagram 搜索平铺、详情在 data.user。
    let n = first_present(v, &["user_info", "user", "data.user"]).unwrap_or(v);

    let handle = pick_str(
        n,
        &["handle", "unique_id", "uniqueId", "username", "channel"],
    )
    // ★ 搜索给 "yykp"，详情给 "@yykp"。不统一的话同一个人会被当成两个。
    .map(|h| h.trim_start_matches('@').to_string());

    let id = pick_str(n, &["id", "channelId", "uid", "pk"]);

    let follower_count = pick_i64(
        n,
        &[
            "subscriberCountInt",     // YouTube 搜索
            "subscriberCount",        // YouTube 详情
            "follower_count",         // TikTok 搜索 / IG merged 形状
            "edge_followed_by.count", // IG 纯 GraphQL 形状
        ],
    )
    // TikTok 详情把粉丝数搬到了平级的 stats 对象里，不在 user 下面
    .or_else(|| pick_i64(v, &["stats.followerCount", "statsV2.followerCount"]));

    let video_count = pick_i64(n, &["media_count", "edge_owner_to_timeline_media.count"])
        .or_else(|| pick_i64(v, &["stats.videoCount"]))
        // YouTube 详情只给 "507 videos" 这种文本，把数字抠出来
        .or_else(|| pick_str(n, &["videoCountText"]).and_then(|t| first_number(&t)));

    if id.is_none() && handle.is_none() {
        return None;
    }
    Some(Creator {
        platform: p,
        id,
        handle,
        name: pick_str(
            n,
            &["channelName", "name", "nickname", "full_name", "title"],
        ),
        bio: pick_str(n, &["description", "signature", "biography"])
            .map(|b| truncate(&b, DESC_LIMIT_CHARS)),
        follower_count,
        video_count,
        verified: n
            .get("is_verified")
            .and_then(Value::as_bool)
            .or_else(|| n.get("isVerified").and_then(Value::as_bool))
            .or_else(|| n.get("verified").and_then(Value::as_bool)),
    })
}

/// 从 "507 videos" 这类文本里抠出第一个数字（会去掉千分位逗号）。
fn first_number(s: &str) -> Option<i64> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// SC 的五个发现类调用。
///
/// 做成 trait 是为了**把网络隔在分发逻辑之外**——参数校验、平台校验、
/// 错误转换这些逻辑全都能用假实现测到，不联网不花钱。
/// 这和第一版把 `LlmClient` 切成 trait 是同一个理由。
pub trait DiscoveryApi {
    fn call(&self, ep: Endpoint, p: Platform, arg: &str) -> Result<RawResponse>;

    /// 抓一条视频的完整内容。走的是第一版那条链路（三个端点 + 转录失败重试），
    /// 和上面四个发现类端点不是一回事，所以单独一个方法。
    fn fetch_video(&self, parsed: &ParsedUrl, raw_url: &str) -> Result<FetchedVideo>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    SearchVideos,
    SearchCreators,
    GetCreator,
    GetCreatorVideos,
}

/// 原始响应 + 实际打的路径。两者都要进 `tool_calls` 表存档。
pub struct RawResponse {
    pub endpoint: String,
    pub body: serde_json::Value,
}

/// 一次 SC 调用要打的路径和参数。
pub struct Route {
    pub path: &'static str,
    pub params: Vec<(&'static str, String)>,
}

/// (工具, 平台) → SC 端点。
///
/// **下面每一条 2026-08-18 都用真 key 调通过**，不是从文档抄的。
/// 上一版设计文档里的端点表凭印象写，错了两处，所以这次逐格实测。
///
/// 返回 `None` 表示这个组合没有可用端点——目前只有
/// `SearchVideos/Instagram` 一种（端点上游失效，见 tools.rs 的说明）。
pub fn route(ep: Endpoint, p: Platform, arg: &str) -> Option<Route> {
    let a = arg.to_string();
    let (path, params): (&str, Vec<(&str, String)>) = match (ep, p) {
        // ★ 搜视频和搜频道是同一个端点，只差 type 参数
        (Endpoint::SearchVideos, Platform::YouTube) => (
            "/v1/youtube/search",
            vec![
                ("query", a),
                ("type", "videos".into()),
                ("sortBy", "popular".into()),
                // includeExtras=true 是拿到 likeCountInt / commentCountInt 的唯一途径
                ("includeExtras", "true".into()),
            ],
        ),
        (Endpoint::SearchCreators, Platform::YouTube) => (
            "/v1/youtube/search",
            vec![
                ("query", a),
                ("type", "channels".into()),
                ("includeExtras", "true".into()),
            ],
        ),
        (Endpoint::SearchVideos, Platform::TikTok) => (
            "/v1/tiktok/search/keyword",
            vec![("query", a), ("sort_by", "most-liked".into())],
        ),
        // Instagram 搜视频：/v2/instagram/reels/search 对所有查询词返回 404
        //（含 SC 文档自己的示例 dogs），/v1/instagram/search/hashtag 同样 404。
        // 没有可用路径，返回 None。修好后在这里加一行即可。
        (Endpoint::SearchVideos, Platform::Instagram) => return None,

        (Endpoint::SearchCreators, Platform::TikTok) => {
            ("/v1/tiktok/search/users", vec![("query", a)])
        }
        // 试错过 /v1/instagram/profile/search 和 /v1/instagram/search/users，都是 Not Found
        (Endpoint::SearchCreators, Platform::Instagram) => {
            ("/v1/instagram/search/profiles", vec![("query", a)])
        }

        (Endpoint::GetCreator, Platform::YouTube) => ("/v1/youtube/channel", vec![("handle", a)]),
        (Endpoint::GetCreator, Platform::TikTok) => ("/v1/tiktok/profile", vec![("handle", a)]),
        (Endpoint::GetCreator, Platform::Instagram) => {
            ("/v1/instagram/profile", vec![("handle", a)])
        }

        (Endpoint::GetCreatorVideos, Platform::YouTube) => {
            ("/v1/youtube/channel-videos", vec![("handle", a)])
        }
        // ★ 是 v3 不是 v1
        (Endpoint::GetCreatorVideos, Platform::TikTok) => {
            ("/v3/tiktok/profile/videos", vec![("handle", a)])
        }
        (Endpoint::GetCreatorVideos, Platform::Instagram) => {
            ("/v1/instagram/user/reels", vec![("handle", a)])
        }
    };
    Some(Route { path, params })
}

impl DiscoveryApi for crate::ingest::scrapecreators::ScrapeCreators {
    fn fetch_video(&self, parsed: &ParsedUrl, raw_url: &str) -> Result<FetchedVideo> {
        self.fetch(parsed, raw_url)
    }

    fn call(&self, ep: Endpoint, p: Platform, arg: &str) -> Result<RawResponse> {
        let Some(rt) = route(ep, p, arg) else {
            return Err(crate::error::ClipKnowError::Fetch {
                platform: p.as_str().into(),
                message: format!("{} 上没有可用的 {ep:?} 端点（SC 侧当前不支持）", p.as_str()),
            });
        };
        let body = self.get_with(rt.path, &rt.params)?;
        Ok(RawResponse {
            endpoint: rt.path.to_string(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::url::Platform;

    fn fx(name: &str) -> serde_json::Value {
        let raw = match name {
            "yt_search_videos" => include_str!("../agent/fixtures/yt_search_videos.json"),
            "tt_search_keyword" => include_str!("../agent/fixtures/tt_search_keyword.json"),
            "tt_profile_videos" => include_str!("../agent/fixtures/tt_profile_videos.json"),
            "ig_user_reels" => include_str!("../agent/fixtures/ig_user_reels.json"),
            "yt_search_channels" => include_str!("../agent/fixtures/yt_search_channels.json"),
            "yt_channel" => include_str!("../agent/fixtures/yt_channel.json"),
            "tt_search_users" => include_str!("../agent/fixtures/tt_search_users.json"),
            "tt_profile" => include_str!("../agent/fixtures/tt_profile.json"),
            "ig_search_profiles" => include_str!("../agent/fixtures/ig_search_profiles.json"),
            "ig_profile_merged" => include_str!("../agent/fixtures/ig_profile_merged.json"),
            "ig_profile_graphql" => include_str!("../agent/fixtures/ig_profile_graphql.json"),
            _ => panic!("没有这个夹具: {name}"),
        };
        serde_json::from_str(raw).expect("夹具不是合法 JSON")
    }

    #[test]
    fn youtube_search_videos_maps_to_video_summaries() {
        let out = parse_search_videos(Platform::YouTube, &fx("yt_search_videos"));

        assert_eq!(out.len(), 2, "夹具里存了 2 条");
        let v = &out[0];
        assert_eq!(v.platform, Platform::YouTube);
        assert_eq!(v.native_id, "rCJX4pPz1_A");
        assert!(v.url.contains("rCJX4pPz1_A"));
        assert!(v.title.as_deref().unwrap().contains("生肉"));
        // 作者信息必须一起带出来 —— 「搜视频再看作者是谁」是找博主的主路径
        assert_eq!(v.channel_name.as_deref(), Some("老高與小茉 Mr & Mrs Gao"));
        assert_eq!(v.channel_handle.as_deref(), Some("laogao"));
        assert_eq!(v.view_count, Some(4_780_866));
    }

    #[test]
    fn youtube_search_uses_publish_date_not_published_time() {
        // 实测：同一条视频 publishedTime=2026-07-18 而 publishDate=2026-06-29，差 19 天。
        // publishedTime 是从「1 month ago」反推的，还可能是 null。
        let out = parse_search_videos(Platform::YouTube, &fx("yt_search_videos"));
        assert!(
            out.iter().all(|v| v.published_at.is_some()),
            "publishDate 始终存在，不该有 None"
        );
    }

    #[test]
    fn tiktok_keyword_search_unwraps_the_aweme_info_layer() {
        // TikTok 搜索的每一项外面还套了一层 search_item_list[].aweme_info，
        // 和 profile/videos 的 aweme_list[] 直接平铺不一样。
        let out = parse_search_videos(Platform::TikTok, &fx("tt_search_keyword"));

        assert_eq!(out.len(), 2);
        let v = &out[0];
        assert_eq!(v.native_id, "7673502248126663954");
        // TikTok 没有独立标题，desc 就是文案
        assert_eq!(v.title.as_deref(), Some("#tiktok #earth #Science "));
        assert_eq!(v.channel_name.as_deref(), Some("科普新视野"));
        assert_eq!(v.channel_handle.as_deref(), Some("unipopsci"));
        // 去重键用 uid（数字串），不是 unique_id
        assert_eq!(v.channel_id.as_deref(), Some("7644867114855760914"));
        assert_eq!(v.view_count, Some(96_872));
        assert_eq!(v.like_count, Some(980), "TikTok 的点赞叫 digg_count");
        assert_eq!(v.comment_count, Some(20));
    }

    #[test]
    fn tiktok_duration_is_milliseconds_not_seconds() {
        // 491519 是毫秒。当成秒会得出 5.7 天。
        let out = parse_search_videos(Platform::TikTok, &fx("tt_search_keyword"));
        assert_eq!(out[0].duration_sec, Some(491));
    }

    #[test]
    fn tiktok_create_time_is_already_unix_seconds() {
        // 和 YouTube 不同，TikTok 不需要解析时间字符串
        let out = parse_search_videos(Platform::TikTok, &fx("tt_search_keyword"));
        assert_eq!(out[0].published_at, Some(1_786_626_474));
    }

    #[test]
    fn tiktok_profile_videos_use_the_flat_aweme_list_shape() {
        // 同一个平台两个端点两种外壳，都要认
        let out = parse_search_videos(Platform::TikTok, &fx("tt_profile_videos"));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].native_id, "7633663452426325278");
        assert_eq!(out[0].duration_sec, Some(47));
    }

    #[test]
    fn instagram_reels_use_code_as_native_id_not_pk() {
        // pk 是 3863794338359282385，但链接里用的是 code。
        // 用 code 才能和第一版 url.rs 解出来的 ID 对上。
        let out = parse_search_videos(Platform::Instagram, &fx("ig_user_reels"));

        assert_eq!(out.len(), 2);
        let v = &out[0];
        assert_eq!(v.native_id, "DWe9Hq_EgrR");
        assert!(v.url.contains("DWe9Hq_EgrR"));
        assert_eq!(v.channel_name.as_deref(), Some("Science 🧬"));
        assert_eq!(v.channel_handle.as_deref(), Some("science"));
        assert_eq!(
            v.channel_id.as_deref(),
            Some("2093326268"),
            "去重用 user.pk"
        );
        assert_eq!(v.view_count, Some(37_851));
        assert_eq!(v.like_count, Some(833));
        assert_eq!(v.published_at, Some(1_774_820_250));
    }

    #[test]
    fn instagram_video_duration_is_a_float_and_gets_floored() {
        // 65.33899688720703 秒 —— 是浮点，直接当整数解析会丢
        let out = parse_search_videos(Platform::Instagram, &fx("ig_user_reels"));
        assert_eq!(out[0].duration_sec, Some(65));
    }

    #[test]
    fn unknown_shape_returns_empty_instead_of_panicking() {
        let out = parse_search_videos(Platform::TikTok, &serde_json::json!({"nothing": true}));
        assert!(out.is_empty());
    }

    // -----------------------------------------------------------------
    // Creator：搜博主 + 博主详情，每家两个端点、字段名都不一样
    // -----------------------------------------------------------------

    #[test]
    fn youtube_search_channels_map_to_creators() {
        let out = parse_search_creators(Platform::YouTube, &fx("yt_search_channels"));
        assert_eq!(out.len(), 2);
        let c = &out[0];
        assert_eq!(c.id.as_deref(), Some("UCoHt-VPaFlVLlCI0zyK85IQ"));
        assert_eq!(c.name.as_deref(), Some("冷科普"));
        assert_eq!(c.follower_count, Some(54_500));
    }

    #[test]
    fn youtube_handle_is_normalized_across_both_endpoints() {
        // 搜索返回 "%E5%86%B7..."（无 @），详情返回 "@hafu"（有 @）。
        // 存进中立类型前统一去掉 @，否则同一个人会被当成两个。
        let searched = parse_search_creators(Platform::YouTube, &fx("yt_search_channels"));
        assert!(!searched[0].handle.as_deref().unwrap().starts_with('@'));

        let detail = parse_creator(Platform::YouTube, &fx("yt_channel")).unwrap();
        assert_eq!(detail.handle.as_deref(), Some("hafu"), "详情里的 @ 要剥掉");
    }

    #[test]
    fn youtube_channel_detail_uses_different_field_names_than_search() {
        // 同一个平台两个端点，四个字段全换了名字：
        //   id→channelId  channelName→name  subscriberCountInt→subscriberCount
        let c = parse_creator(Platform::YouTube, &fx("yt_channel")).unwrap();
        assert_eq!(c.id.as_deref(), Some("UCXMVaxrax7RNDPdfRrXXgtQ"));
        assert_eq!(c.name.as_deref(), Some("Hafu Go"));
        assert_eq!(c.follower_count, Some(19_100_000));
        assert_eq!(
            c.video_count,
            Some(507),
            "videoCountText 是 \"507 videos\"，要抠出数字"
        );
    }

    #[test]
    fn tiktok_search_users_map_to_creators() {
        let out = parse_search_creators(Platform::TikTok, &fx("tt_search_users"));
        assert_eq!(out.len(), 2);
        let c = &out[0];
        assert_eq!(c.id.as_deref(), Some("7644867114855760914"), "去重用 uid");
        assert_eq!(c.handle.as_deref(), Some("unipopsci"));
        assert_eq!(c.name.as_deref(), Some("科普新视野"));
        assert_eq!(c.follower_count, Some(11_264));
    }

    #[test]
    fn tiktok_profile_detail_switches_to_camel_case_and_a_separate_stats_object() {
        // 搜索里是 user_info.unique_id / follower_count（下划线）
        // 详情里是 user.uniqueId（驼峰），粉丝数还搬去了 stats.followerCount
        let c = parse_creator(Platform::TikTok, &fx("tt_profile")).unwrap();
        assert_eq!(c.id.as_deref(), Some("6600107989528985606"));
        assert_eq!(c.handle.as_deref(), Some("fallontonight"));
        assert_eq!(c.follower_count, Some(29_900_000));
        assert_eq!(c.video_count, Some(5_525));
    }

    #[test]
    fn instagram_search_profiles_map_to_creators() {
        let out = parse_search_creators(Platform::Instagram, &fx("ig_search_profiles"));
        let c = &out[0];
        assert_eq!(c.id.as_deref(), Some("414805671"));
        assert_eq!(c.handle.as_deref(), Some("natgeoscience"));
        assert_eq!(c.follower_count, Some(9_925_182));
        assert_eq!(c.verified, Some(true));
    }

    #[test]
    fn instagram_profile_handles_the_graphql_shape() {
        // 实测：natgeoscience / nasa 返回 69 键的纯 GraphQL 形状，
        // 没有 pk、没有 follower_count，粉丝数在 edge_followed_by.count 里
        let c = parse_creator(Platform::Instagram, &fx("ig_profile_graphql")).unwrap();
        assert_eq!(c.id.as_deref(), Some("414805671"));
        assert_eq!(c.handle.as_deref(), Some("natgeoscience"));
        assert_eq!(c.follower_count, Some(9_925_202));
    }

    #[test]
    fn instagram_profile_handles_the_merged_shape_too() {
        // 实测：science 返回 111 键，mobile 和 GraphQL 字段同时存在。
        // 同一个端点两种形状都在生产里出现，两种都得认。
        let c = parse_creator(Platform::Instagram, &fx("ig_profile_merged")).unwrap();
        assert_eq!(c.id.as_deref(), Some("2093326268"));
        assert_eq!(c.handle.as_deref(), Some("science"));
        assert!(c.follower_count.unwrap() > 1_000_000);
    }

    // -----------------------------------------------------------------
    // 端点路由。下面每一条 2026-08-18 都用真 key 调通过。
    // -----------------------------------------------------------------

    fn r(ep: Endpoint, p: Platform) -> Route {
        route(ep, p, "X").unwrap_or_else(|| panic!("{ep:?}/{} 没有路由", p.as_str()))
    }

    fn param(rt: &Route, k: &str) -> Option<String> {
        rt.params
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn youtube_search_uses_one_endpoint_with_a_type_switch() {
        // 搜视频和搜频道是**同一个端点**，只差 type 参数
        let v = r(Endpoint::SearchVideos, Platform::YouTube);
        let c = r(Endpoint::SearchCreators, Platform::YouTube);
        assert_eq!(v.path, "/v1/youtube/search");
        assert_eq!(c.path, "/v1/youtube/search");
        assert_eq!(param(&v, "type").as_deref(), Some("videos"));
        assert_eq!(param(&c, "type").as_deref(), Some("channels"));
    }

    #[test]
    fn search_video_paths_match_what_was_verified() {
        assert_eq!(
            r(Endpoint::SearchVideos, Platform::TikTok).path,
            "/v1/tiktok/search/keyword"
        );
    }

    #[test]
    fn instagram_video_search_has_no_route_because_the_endpoint_is_down() {
        // 实测 404（含 SC 文档自己的示例 dogs）。没有路由 = 打不出去。
        assert!(route(Endpoint::SearchVideos, Platform::Instagram, "x").is_none());
    }

    #[test]
    fn search_creator_paths_match_what_was_verified() {
        assert_eq!(
            r(Endpoint::SearchCreators, Platform::TikTok).path,
            "/v1/tiktok/search/users"
        );
        assert_eq!(
            r(Endpoint::SearchCreators, Platform::Instagram).path,
            "/v1/instagram/search/profiles",
            "试错过 /v1/instagram/profile/search 和 /v1/instagram/search/users，都是 Not Found"
        );
    }

    #[test]
    fn get_creator_paths_match_what_was_verified() {
        assert_eq!(
            r(Endpoint::GetCreator, Platform::YouTube).path,
            "/v1/youtube/channel"
        );
        assert_eq!(
            r(Endpoint::GetCreator, Platform::TikTok).path,
            "/v1/tiktok/profile"
        );
        assert_eq!(
            r(Endpoint::GetCreator, Platform::Instagram).path,
            "/v1/instagram/profile"
        );
    }

    #[test]
    fn get_creator_videos_paths_match_what_was_verified() {
        assert_eq!(
            r(Endpoint::GetCreatorVideos, Platform::YouTube).path,
            "/v1/youtube/channel-videos"
        );
        assert_eq!(
            r(Endpoint::GetCreatorVideos, Platform::TikTok).path,
            "/v3/tiktok/profile/videos",
            "是 v3 不是 v1"
        );
        assert_eq!(
            r(Endpoint::GetCreatorVideos, Platform::Instagram).path,
            "/v1/instagram/user/reels"
        );
    }

    #[test]
    fn search_endpoints_send_query_and_creator_endpoints_send_handle() {
        assert_eq!(
            param(&r(Endpoint::SearchVideos, Platform::TikTok), "query").as_deref(),
            Some("X")
        );
        assert_eq!(
            param(&r(Endpoint::GetCreator, Platform::TikTok), "handle").as_deref(),
            Some("X")
        );
        assert!(param(&r(Endpoint::GetCreator, Platform::TikTok), "query").is_none());
    }

    #[test]
    fn youtube_search_always_sets_include_extras() {
        // includeExtras=true 是拿到 likeCountInt / commentCountInt 的唯一途径
        let v = r(Endpoint::SearchVideos, Platform::YouTube);
        assert_eq!(param(&v, "includeExtras").as_deref(), Some("true"));
    }
}

//! 把库里的视频数据拼成给模型看的「证据」文本。
//!
//! 第一版就是老老实实把所有东西拼成一段文字塞进 prompt——
//! 一个十分钟视频的转录也就几千 token，全塞进去比任何检索都准也都便宜。
//! （切块、向量化、召回 top-k 在单视频问答里只会**丢信息**：召回漏掉的
//! 段落模型就看不到了。这就是为什么第一版不上 RAG。）

use crate::content::model::Video;
use crate::store::StoredVideo;

/// 转录超过这个字符数就截断。
///
/// 设计上更好的做法是分段摘要，但那需要多次模型调用。第一版先截断，
/// 并在给模型的文本和给用户的输出里都明确标注，不假装完整。
pub const TRANSCRIPT_LIMIT_CHARS: usize = 40_000;

/// 给模型的系统提示词。
pub const SYSTEM_PROMPT: &str = "\
你是一个社交媒体视频内容分析助手。用户会给你一个视频的元数据、文字稿和评论，然后提问。

要求：
- 只根据给到的材料回答。材料里没有的信息，直接说没有，不要猜测或编造。
- 如果文字稿是空的或明显不完整，如实告诉用户这一点，不要硬凑一个答案。
- 如果文字稿被标注为「已截断」，在回答里提醒用户你只看到了前面一部分。
- 回答用中文，简洁直接，先给结论再给依据。";

/// 把一个视频的全部材料拼成一段文本。
pub fn build_evidence(sv: &StoredVideo) -> String {
    let mut out = String::new();

    out.push_str("=== 视频信息 ===\n");
    out.push_str(&format_metadata(&sv.video));

    out.push_str("\n=== 文字稿 ===\n");
    match &sv.transcript {
        Some(t) if !t.text.trim().is_empty() => {
            let chars: Vec<char> = t.text.chars().collect();
            if chars.len() > TRANSCRIPT_LIMIT_CHARS {
                let head: String = chars[..TRANSCRIPT_LIMIT_CHARS].iter().collect();
                out.push_str(&head);
                out.push_str(&format!(
                    "\n\n[注意：文字稿已截断。原文共 {} 字，这里只给出前 {} 字。]\n",
                    chars.len(),
                    TRANSCRIPT_LIMIT_CHARS
                ));
            } else {
                out.push_str(&t.text);
                out.push('\n');
            }
        }
        _ => {
            out.push_str("（没有文字稿。这个视频可能没有语音内容，或者平台没有提供字幕。）\n");
        }
    }

    if !sv.comments.is_empty() {
        out.push_str(&format!("\n=== 评论（{} 条，按点赞数排序）===\n", sv.comments.len()));
        for c in &sv.comments {
            let who = c.author.as_deref().unwrap_or("匿名");
            match c.like_count {
                Some(n) => out.push_str(&format!("- [{n} 赞] {who}: {}\n", c.text)),
                None => out.push_str(&format!("- {who}: {}\n", c.text)),
            }
        }
    }

    out
}

fn format_metadata(v: &Video) -> String {
    let mut s = String::new();
    s.push_str(&format!("平台: {}\n", v.platform.as_str()));
    if let Some(t) = &v.title {
        s.push_str(&format!("标题: {t}\n"));
    }
    match (&v.author_name, &v.author_handle) {
        (Some(n), Some(h)) => s.push_str(&format!("作者: {n} (@{h})\n")),
        (Some(n), None) => s.push_str(&format!("作者: {n}\n")),
        (None, Some(h)) => s.push_str(&format!("作者: @{h}\n")),
        (None, None) => {}
    }
    if let Some(d) = v.duration_sec {
        s.push_str(&format!("时长: {}\n", format_duration(d)));
    }
    if let Some(ts) = v.published_at {
        s.push_str(&format!("发布时间: {}\n", format_date(ts)));
    }
    let stats: Vec<String> = [
        v.view_count.map(|n| format!("播放 {n}")),
        v.like_count.map(|n| format!("点赞 {n}")),
        v.comment_count.map(|n| format!("评论 {n}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !stats.is_empty() {
        s.push_str(&format!("数据: {}\n", stats.join(" / ")));
    }
    if let Some(d) = &v.description {
        let d = d.trim();
        if !d.is_empty() {
            s.push_str(&format!("简介: {d}\n"));
        }
    }
    s
}

pub fn format_duration(secs: i64) -> String {
    let (m, s) = (secs / 60, secs % 60);
    if m >= 60 {
        format!("{}:{:02}:{:02}", m / 60, m % 60, s)
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn format_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| ts.to_string())
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::{Comment, Transcript, Video};
    use crate::ingest::url::Platform;

    fn video() -> Video {
        Video {
            id: "v1".into(),
            platform: Platform::YouTube,
            native_id: "abc".into(),
            url: "https://x".into(),
            title: Some("如何获取第一批用户".into()),
            author_handle: Some("someone".into()),
            author_name: Some("Some One".into()),
            duration_sec: Some(632),
            published_at: Some(1772402418),
            view_count: Some(231),
            like_count: Some(1),
            comment_count: None,
            description: Some("聊聊冷启动".into()),
            raw_json: "{}".into(),
            fetched_at: 0,
        }
    }

    fn stored(transcript: Option<&str>, comments: Vec<Comment>) -> StoredVideo {
        StoredVideo {
            video: video(),
            transcript: transcript.map(|t| Transcript {
                video_id: "v1".into(),
                text: t.into(),
                source: "sc".into(),
                lang: Some("Chinese".into()),
                fetched_at: 0,
            }),
            comments,
        }
    }

    #[test]
    fn includes_metadata_transcript_and_comments() {
        let sv = stored(
            Some("大家好，今天聊冷启动。"),
            vec![Comment {
                id: "c1".into(), video_id: "v1".into(), author: Some("张三".into()),
                text: "很有用".into(), like_count: Some(42), published_at: None,
            }],
        );
        let e = build_evidence(&sv);

        assert!(e.contains("如何获取第一批用户"), "要有标题");
        assert!(e.contains("Some One (@someone)"), "要有作者");
        assert!(e.contains("10:32"), "632 秒要格式化成 10:32");
        assert!(e.contains("2026-03-01"), "要有发布日期");
        assert!(e.contains("播放 231"), "要有播放量");
        assert!(e.contains("大家好，今天聊冷启动。"), "要有文字稿");
        assert!(e.contains("[42 赞] 张三: 很有用"), "要有评论和点赞数");
    }

    #[test]
    fn says_so_explicitly_when_transcript_is_missing() {
        // TikTok/IG 上纯画面+BGM 的内容会走到这里。
        // 必须让模型知道「没有」，而不是留空让它自由发挥。
        let e = build_evidence(&stored(None, vec![]));
        assert!(e.contains("没有文字稿"), "缺文字稿必须显式说明，实际输出:\n{e}");
    }

    #[test]
    fn empty_transcript_treated_same_as_missing() {
        let e = build_evidence(&stored(Some("   "), vec![]));
        assert!(e.contains("没有文字稿"));
    }

    #[test]
    fn truncates_overlong_transcript_and_labels_it() {
        let long = "字".repeat(TRANSCRIPT_LIMIT_CHARS + 500);
        let e = build_evidence(&stored(Some(&long), vec![]));
        assert!(e.contains("已截断"), "截断必须标注出来，不能假装完整");
        assert!(e.contains(&format!("原文共 {} 字", TRANSCRIPT_LIMIT_CHARS + 500)));
    }

    #[test]
    fn does_not_truncate_transcript_at_the_limit() {
        let exact = "字".repeat(TRANSCRIPT_LIMIT_CHARS);
        let e = build_evidence(&stored(Some(&exact), vec![]));
        assert!(!e.contains("已截断"), "刚好到上限不该截断");
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        // 中文一个字 3 字节。如果按字节切会把字符劈成两半导致 panic，
        // 这个测试就是防这个。
        let long = "中".repeat(TRANSCRIPT_LIMIT_CHARS + 10);
        let e = build_evidence(&stored(Some(&long), vec![]));
        assert!(e.contains("已截断"));
    }

    #[test]
    fn comments_section_omitted_when_empty() {
        let e = build_evidence(&stored(Some("有内容"), vec![]));
        assert!(!e.contains("=== 评论"), "没评论就不该有评论段落");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(57), "0:57");
        assert_eq!(format_duration(632), "10:32");
        assert_eq!(format_duration(3661), "1:01:01");
    }
}

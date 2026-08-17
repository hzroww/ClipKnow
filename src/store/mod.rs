//! 存储层。
//!
//! 对外只暴露 `Store` 这一个 trait。第一版底下是 SQLite，
//! 以后换 Postgres 只需要再写一个实现，上层代码一行不动。
//!
//! trait 就是 C++ 里的抽象基类：定义「能做什么」，不管「怎么做」。

pub mod sqlite;

use crate::content::model::{Comment, FetchedVideo, Transcript, Video};
use crate::error::Result;
use crate::ingest::url::Platform;

/// 从库里读出来的一个视频的全部内容。
#[derive(Debug, Clone)]
pub struct StoredVideo {
    pub video: Video,
    pub transcript: Option<Transcript>,
    pub comments: Vec<Comment>,
}

pub trait Store {
    /// 按 (平台, 平台内 ID) 查一个视频。抓过就能命中，省一次 SC 的钱。
    fn find_by_native(&self, platform: Platform, native_id: &str) -> Result<Option<StoredVideo>>;

    /// 把一次抓取的结果整个写进去（视频 + 文字稿 + 评论）。
    /// 已存在同一个视频时覆盖，返回库里那条记录的 id。
    fn save(&mut self, fetched: &FetchedVideo) -> Result<String>;

    /// 列出库里所有视频，最近抓的排前面。
    fn list_videos(&self, limit: usize) -> Result<Vec<Video>>;
}

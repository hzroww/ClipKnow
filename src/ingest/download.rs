//! 把一个视频直链下载成内存里的字节。
//!
//! **不落磁盘。** 拿到的 `Vec<u8>` 编码成 base64 发给视觉模型，用完就 drop。
//! 这样不需要临时文件、不需要清理逻辑、不需要磁盘配额——整个项目除了
//! SQLite 之外仍然不碰本地文件。
//!
//! 为什么需要这一层（而不是把直链交给视觉模型自己去拉）：
//!   - 千问的 `video_url` 只能拉到**它的机房到得了**的地址。实测阿里云
//!     到得了 `*.tiktokcdn-eu.com`（7/7 成功），到不了 `*.tiktokcdn-us.com`
//!     （0/9，全部 `Download multimodal file timed out`）和
//!     `cdninstagram.com`（连几十 KB 的图片都超时）。
//!   - Gemini 压根不接受第三方 URL，只认 YouTube 链接和自家 Files API。
//!
//! 所以第一版统一走「自己下载」这一条路：一条代码路径，不依赖任何供应商
//! 的 CDN 可达性，也不依赖平台的 CDN 区域分配规律（那个规律会变）。
//! 以后想给可达的地址加回「零下载」快路，是一行 host 判断的事。

use std::io::Read;
use std::time::Duration;

use crate::error::{ClipKnowError, Result};

/// 下载上限。实测样本里最大的一条 TikTok 是 58MB，留一倍余量。
///
/// 必须有上限：直链是从平台响应里解出来的，一个 2GB 的长视频会把内存
/// 吃光、把请求体撑爆。超限时返回明确错误，让模型知道这条视频太大，
/// 而不是让进程 OOM。
pub const MAX_VIDEO_BYTES: usize = 100 * 1024 * 1024;

/// 下载超时。实测速度约 1–2 MB/s，100MB 需要 50–100 秒，给到 180 秒。
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 180;

/// 下载器。抽成 trait 只为了测试能塞假实现——真实现只有一个。
pub trait VideoFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>>;
}

/// 真实现：裸 HTTP GET。
///
/// **刻意不带任何请求头。** 实测 TikTok 的 CDN 直链不带 User-Agent、
/// 不带 Referer 也返回 206，Instagram 同样。伪装成浏览器反而可能踩到
/// 更严的反爬策略（抖音就是这样：带浏览器 UA 只返回 72KB 的 JS 挑战，
/// 不带 UA 反而给完整页面）。
pub struct HttpVideoFetcher {
    http: reqwest::blocking::Client,
    max_bytes: usize,
}

impl HttpVideoFetcher {
    pub fn new() -> Self {
        Self::with_limit(MAX_VIDEO_BYTES)
    }

    pub fn with_limit(max_bytes: usize) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
            .build()
            .expect("构造 HTTP 客户端失败");
        Self { http, max_bytes }
    }
}

impl Default for HttpVideoFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoFetcher for HttpVideoFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.http.get(url).send()?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ClipKnowError::Fetch {
                platform: "video-cdn".into(),
                message: format!("HTTP {status}"),
            });
        }

        // ★ 先看 Content-Length。有的话就能在**下载之前**拒掉超大文件，
        //   而不是先拉 100MB 再发现不行。
        if let Some(len) = resp.content_length()
            && len as usize > self.max_bytes
        {
            return Err(too_big(len as usize, self.max_bytes));
        }

        // Content-Length 可能缺失（chunked），所以边读边卡上限：
        // 多读 1 个字节，读满了就说明超限。
        let mut buf = Vec::new();
        let mut limited = resp.take(self.max_bytes as u64 + 1);
        limited.read_to_end(&mut buf)?;
        if buf.len() > self.max_bytes {
            return Err(too_big(buf.len(), self.max_bytes));
        }
        if buf.is_empty() {
            return Err(ClipKnowError::Fetch {
                platform: "video-cdn".into(),
                message: "下载到 0 字节".into(),
            });
        }
        Ok(buf)
    }
}

fn too_big(actual: usize, limit: usize) -> ClipKnowError {
    ClipKnowError::Fetch {
        platform: "video-cdn".into(),
        message: format!(
            "视频 {:.1}MB，超过 {}MB 下载上限",
            actual as f64 / 1_048_576.0,
            limit / 1_048_576
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_message_names_both_numbers() {
        // 给模型看的错误必须能行动：说清「多大」和「上限多少」，
        // 它才知道这是视频太大而不是网络问题，也就不会重试。
        let e = too_big(340 * 1_048_576, MAX_VIDEO_BYTES).to_string();
        assert!(e.contains("340.0MB"), "要说实际大小: {e}");
        assert!(e.contains("100MB"), "要说上限: {e}");
    }

    /// 实测样本最大 58MB（TikTok 科普类长视频）。上限贴着样本设会在遇到
    /// 稍长的视频时立刻失效，所以留一倍余量。编译期钉住。
    const _: () = assert!(MAX_VIDEO_BYTES >= 90 * 1_048_576);

    /// 测试用的假下载器：按 URL 返回预设字节或预设错误。
    pub struct FakeFetcher {
        pub bytes: Vec<u8>,
        pub fail: Option<String>,
    }

    impl VideoFetcher for FakeFetcher {
        fn fetch(&self, _url: &str) -> Result<Vec<u8>> {
            match &self.fail {
                Some(m) => Err(ClipKnowError::Fetch {
                    platform: "video-cdn".into(),
                    message: m.clone(),
                }),
                None => Ok(self.bytes.clone()),
            }
        }
    }

    #[test]
    fn a_fake_fetcher_can_stand_in_for_the_network() {
        let f = FakeFetcher {
            bytes: vec![1, 2, 3],
            fail: None,
        };
        assert_eq!(f.fetch("whatever").unwrap(), vec![1, 2, 3]);

        let f = FakeFetcher {
            bytes: vec![],
            fail: Some("下载超时".into()),
        };
        assert!(f.fetch("whatever").is_err());
    }
}

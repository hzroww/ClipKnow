//! 全项目统一的错误类型。
//!
//! Rust 没有异常，函数用 `Result<T, E>` 返回「要么成功要么失败」。
//! 这里用 `thiserror` 定义错误枚举，它会自动生成错误信息的显示代码
//! （相当于 C++ 里给异常类写 `what()`，但不用手写）。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipKnowError {
    #[error("无法识别的链接: {0}\n目前只支持 YouTube / TikTok / Instagram")]
    UnsupportedUrl(String),

    #[error("链接里找不到视频 ID: {0}")]
    NoVideoId(String),

    #[error("抓取失败 ({platform}): {message}")]
    Fetch { platform: String, message: String },

    #[error("大模型调用失败: {0}")]
    Llm(String),

    #[error("模型拒绝回答（stop_reason=refusal）")]
    LlmRefusal,

    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("网络错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),

    #[error("缺少环境变量 {0}，请在 ~/.zshrc 里设置后重开终端")]
    MissingEnv(&'static str),
}

/// 项目内部统一用这个 Result，少写一遍错误类型。
pub type Result<T> = std::result::Result<T, ClipKnowError>;

//! ClipKnow —— 社媒视频内容分析工具。
//!
//! 这个文件是「库」的入口，只负责声明有哪些模块。
//! 真正的命令行程序在 `main.rs`，它把这个库当外部依赖来用
//! （所以 main.rs 里写的是 `use clipknow::...`）。
//!
//! 这样拆分的好处：库的部分可以被测试直接调用，也能在以后
//! 加 HTTP 服务时被 axum 的 handler 调用，不用改动任何逻辑。

pub mod error;
pub mod ingest;

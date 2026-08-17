//! ClipKnow —— 社媒视频内容分析工具。
//!
//! 这个文件是「库」的入口，只负责声明有哪些模块。
//! 真正的命令行程序在 `main.rs`，它把这个库当外部依赖用
//! （所以 main.rs 里写 `use clipknow::...`）。

pub mod agent;
pub mod content;
pub mod error;
pub mod ingest;
pub mod store;

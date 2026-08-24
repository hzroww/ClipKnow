//! Agent 层。
//!
//! 第一版这里只有 llm.rs（大模型客户端）——单视频问答一次调用就够了，
//! 不需要 agent 循环。第二步做「帮我找几个做科普的博主」这类需求时，
//! 才会加 runner.rs（循环）和 tools.rs（工具集）：那时候要查哪几个频道
//! 取决于上一步搜到了什么，没法提前写死，循环才真正必要。

pub mod compaction;
pub mod context;
pub mod llm;
pub mod runner;
pub mod tools;

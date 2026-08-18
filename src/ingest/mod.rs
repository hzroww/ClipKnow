//! 采集层：脏活隔离区。
//!
//! 这一层负责「把一个链接变成结构化数据」，是整个项目里最容易因为
//! 外部平台改动而坏掉的部分。上层只通过这里暴露的类型跟它打交道——
//! SC 挂了或者要换供应商，只改这个目录，store 和 llm 一行不动。

pub mod discovery;
pub mod scrapecreators;
pub mod url;

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
/// Rust ↔ Go 的线格式。见模块文档。
pub mod wire;

/// 读环境变量，**空字符串当作没设**。
///
/// `std::env::var` 对「设成空串」返回 `Ok("")`，于是
/// `env::var("X").unwrap_or_else(|_| 默认值)` 里的默认值永远用不上——拿到的是空串。
///
/// 这个坑是从 docker-compose 里踩出来的:
///
/// ```yaml
/// DEEPSEEK_MODEL: ${DEEPSEEK_MODEL:-}     # 宿主没设 → 容器里是空串，不是没设
/// ```
///
/// 结果模型名是空的，请求直接被 API 拒掉，而报错跟「模型名」一个字都不沾。
/// compose 那边已经改成「只写变量名」的写法（没设就完全不传），但**代码不能靠
/// 调用方永远写对**：`.env` 里写一行 `DEEPSEEK_MODEL=` 是同样的效果，而那是
/// 任何人都可能犯的错。
///
/// 顺带也 trim：`DASHSCOPE_API_KEY=" "` 这种更难查——它会让程序以为配了视觉模型，
/// 于是不走「未配置」那条降级路径，而是拿一个空 key 去请求，最后报的是认证失败。
pub fn env_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod env_var_tests {
    use super::env_var;

    // 每个断言用**独一无二的变量名**，这样并行跑测试时互不干扰——
    // 改进程环境是全局操作，共用一个名字必然打架。
    fn probe(name: &str, set_to: Option<&str>) -> Option<String> {
        // SAFETY: 名字是这个测试独占的，没有别的线程读它。
        unsafe {
            match set_to {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        env_var(name)
    }

    #[test]
    fn 没设返回_none() {
        assert_eq!(probe("CLIPKNOW_T_UNSET", None), None);
    }

    #[test]
    fn 空串当作没设() {
        // 这就是 docker-compose 的 ${VAR:-} 造成的情况。
        assert_eq!(probe("CLIPKNOW_T_EMPTY", Some("")), None);
    }

    #[test]
    fn 只有空白也当作没设() {
        assert_eq!(probe("CLIPKNOW_T_BLANK", Some("   ")), None);
    }

    #[test]
    fn 有值就原样返回_不_trim() {
        // 只用 trim 判断「是不是空」，返回的仍是原值——key 万一真带前后空格，
        // 悄悄改掉它会让「我明明配对了」变成更难查的问题。
        assert_eq!(
            probe("CLIPKNOW_T_VALUE", Some(" sk-abc ")),
            Some(" sk-abc ".to_string())
        );
    }
}

//! 一个失效的 `oss://` 引用，千问报什么错？
//!
//! 这决定了 `is_permanent_failure` 会不会把「临时上传过期」误判成
//! 「这条视频永远不行」。误判的后果是 blocked_reason 落库，视频永久封禁。
use clipknow::agent::vision::{build_vision_client, is_permanent_failure};

fn main() {
    let _ = dotenvy::dotenv();
    let Some(v) = build_vision_client() else {
        eprintln!("没有 DASHSCOPE_API_KEY");
        return;
    };

    // 格式合法但对象不存在的引用，模仿真实的那一种：
    //   oss://dashscope-instant/<32位hex>/video.mp4
    let cases = [
        (
            "格式对但对象不存在",
            "oss://dashscope-instant/1a2b3c4d5e6f708192a3b4c5d6e7f809/video.mp4",
        ),
        (
            "路径明显不对",
            "oss://dashscope-instant/nope/nothing-here.mp4",
        ),
    ];

    for (label, reference) in cases {
        print!("{label:<22}");
        match v.analyze(reference, Some(30), None) {
            Ok(r) => println!("竟然成功了？{} 字", r.text.chars().count()),
            Err(e) => {
                let msg = e.to_string();
                let perm = is_permanent_failure(&e);
                println!(
                    "{}\n{:22}判定：{}",
                    msg.chars().take(300).collect::<String>(),
                    "",
                    if perm {
                        "★★ 永久 —— 会落 blocked_reason，视频永久封禁"
                    } else {
                        "可重试 —— 不落 blocked_reason"
                    }
                );
            }
        }
        println!();
    }
}

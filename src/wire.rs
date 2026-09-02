//! Rust ↔ Go 之间的线格式：**每行一个 JSON**(NDJSON)。
//!
//! ## 为什么是这个形状
//!
//! web 服务是 Go 写的,而 agent 循环在 Rust 这边。两边通信有三条路可选:
//! CGO 调动态库、常驻服务 + socket、**子进程 + stdout**。选了第三条:
//!
//!   - 不用 FFI,不用手写 `extern "C"` 边界和字符串内存管理
//!   - 崩溃隔离:视觉管线 panic 只死掉这一次提问,web 服务照样活着
//!   - **可单独调试**:把 Go 摘掉,手敲 `clipknow turn --json ...`,
//!     肉眼就能看出 Rust 到底吐了什么
//!   - 进程启动约 10 毫秒,而一次 turn 是 15–60 **秒**——这点开销是噪声
//!
//! 一行一个 JSON 而不是一整个 JSON 数组,是因为要**边产生边发**:Go 那边
//! `bufio.Scanner` 读一行就能立刻转成一个 SSE 事件推给浏览器。整个数组
//! 得等结束才能解析,那就没有流式可言了。
//!
//! ## 不变量
//!
//! 1. **stdout 只有 NDJSON。** 人看的日志一律走 stderr。库里现在 5 处输出
//!    全是 `eprintln!`,一个 `println!` 都没有——加的时候别破坏这条。
//! 2. **每行写完立刻 flush。** 不 flush 就攒在缓冲区里,等于没有流式。
//! 3. **`t` 字段是判别标签**,Go 靠它分派。加新事件类型不影响老的解析。
//! 4. **`PROTOCOL_VERSION` 变了才算破坏兼容。** 加字段不算(Go 忽略不认识的),
//!    改字段含义、删字段、改 `t` 或 `outcome` 的取值才算。
//!
//! ## 流式 token（已实现）
//!
//! `answer` **之前**会有一串 `{"t":"token","text":"..."}`。加这个的时候
//! Go 和前端的结构都没改——协议当初就是按「加事件类型不影响老的解析」
//! 设计的，Go 逐行转发、不理解内容。
//!
//! ⚠️ **中间轮次也有 token。** 模型发起工具调用时常常同时说一句话。所以
//! 界面在收到 `tool_call` 之前无法判断刚才那段是中间话还是最终答案——
//! 先流进待定区，等下一个事件再决定。`answer` 仍然要留着做校准和确认。

use std::cell::RefCell;
use std::io::Write;

use serde_json::{Value, json};

use crate::agent::runner::{TurnEvent, TurnObserver, TurnOutcome, TurnResult};

/// 协议版本。Go 启动子进程后先读 `hello`,版本对不上就直接报错,
/// 而不是在某个字段上得到 `null` 然后诡异地跑下去。
pub const PROTOCOL_VERSION: u32 = 1;

/// `TurnOutcome` 的稳定字符串。
///
/// **Go 那边会 match 这些值,所以它们是协议的一部分**——改一个字面量就是
/// 破坏性变更,要连 `PROTOCOL_VERSION` 一起动。枚举变体名可以随便改,
/// 这个映射函数是唯一的对外契约。
pub fn outcome_tag(o: &TurnOutcome) -> &'static str {
    match o {
        TurnOutcome::Done => "done",
        TurnOutcome::IterationCap => "iteration_cap",
        TurnOutcome::Truncated => "truncated",
        TurnOutcome::ProtocolError(_) => "protocol_error",
        TurnOutcome::ContextBudget { .. } => "context_budget",
        TurnOutcome::ModelError(_) => "model_error",
    }
}

/// 循环事件 → 一行 JSON。
///
/// 纯函数,单独抽出来是为了能**离线单元测试协议形状**——这一层最容易在
/// 改字段名时悄悄坏掉,而它坏了的表现是 Go 那边某个字段变成 undefined、
/// 界面少显示一块东西,不会报错。
pub fn event_json(ev: &TurnEvent<'_>) -> Value {
    match ev {
        TurnEvent::Compacted {
            summary_chars,
            upto_seq,
        } => json!({"t": "compacted", "summary_chars": summary_chars, "upto_seq": upto_seq}),
        TurnEvent::Iteration { n } => json!({"t": "iteration", "n": n}),
        TurnEvent::ToolCall { id, name, args } => {
            json!({"t": "tool_call", "id": id, "name": name, "args": args})
        }
        TurnEvent::ToolResult {
            id,
            name,
            is_error,
            external_calls,
            credits,
            vision_calls,
            preview,
        } => json!({
            "t": "tool_result",
            "id": id,
            "name": name,
            "is_error": is_error,
            // ★ 这三个是**这一次调用**的增量,不是累计值。
            //   累计值在最后的 usage 行里。
            "external_calls": external_calls,
            "credits": credits,
            "vision_calls": vision_calls,
            "preview": preview,
        }),
        TurnEvent::Token { text } => json!({"t": "token", "text": text}),
        TurnEvent::Answer { text } => {
            json!({"t": "answer", "text": crate::content::evidence::with_signature(text)})
        }
    }
}

/// 开场那一行。Go 读到它才知道自己在跟什么说话。
pub fn hello_json(session_id: &str, model: &str, vision_model: Option<&str>) -> Value {
    json!({
        "t": "hello",
        "protocol": PROTOCOL_VERSION,
        "session": session_id,
        "model": model,
        // null 明确表示「没配视觉模型」,和「配了但叫 null」区分得开
        "vision": vision_model,
    })
}

/// 收尾的用量行。**累计值**在这里,不在 `tool_result` 里。
pub fn usage_json(res: &TurnResult, cost_usd: f64) -> Value {
    json!({
        "t": "usage",
        "iterations": res.iterations,
        // 注意:这是**外部端点调用次数**,不是模型发起的工具调用次数。
        // fetch_video 一次调用打 3 个端点,命中缓存打 0 个。
        "external_calls": res.tool_calls_made,
        "credits": res.credits_charged,
        "context_tokens": res.context_tokens,
        "compactions": res.compactions,
        "video_analyses": res.video_analyses,
        "video_tokens": res.video_tokens,
        "input_tokens": res.input_tokens,
        "cached_input_tokens": res.cached_input_tokens,
        "output_tokens": res.output_tokens,
        "cost_usd": cost_usd,
    })
}

/// 终止行。`outcome` 之外再带一句人能读的话——界面直接显示它,
/// 不用在 Go 或前端里再维护一份「哪个 outcome 该说什么」的映射。
pub fn done_json(o: &TurnOutcome, note: &str) -> Value {
    json!({"t": "done", "outcome": outcome_tag(o), "note": note})
}

/// 出错行。子进程在**跑循环之前**就失败时用(开库失败、没配 key 之类)。
pub fn error_json(message: &str) -> Value {
    json!({"t": "error", "message": message})
}

/// 把事件写成 NDJSON。
///
/// `RefCell` 是因为 `TurnObserver::on` 拿的是 `&self`——观察者不能改循环的
/// 任何决定,所以签名里没有 `&mut`。而写文件天然需要可变引用,只好在这里
/// 用内部可变性把它包起来。单线程,不会有借用冲突。
pub struct NdjsonSink<W: Write> {
    out: RefCell<W>,
}

impl<W: Write> NdjsonSink<W> {
    pub fn new(out: W) -> Self {
        NdjsonSink {
            out: RefCell::new(out),
        }
    }

    /// 写一行并 flush。
    ///
    /// **写失败一律忽略**。这是刻意的:stdout 断了(Go 那边关掉了、浏览器
    /// 断开了)不该让一次正在跑的分析崩掉——它已经花了钱,结果要落库。
    /// 真正该中止的信号是子进程被杀,那由操作系统管。
    pub fn emit(&self, v: &Value) {
        let mut out = self.out.borrow_mut();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }
}

impl<W: Write> TurnObserver for NdjsonSink<W> {
    fn on(&self, ev: &TurnEvent<'_>) {
        self.emit(&event_json(ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个事件都必须有 `t`,而且 Go 那边靠它分派。漏一个就是静默失效。
    #[test]
    fn every_event_carries_a_discriminator() {
        let args = json!({"url": "https://x"});
        let evs = [
            TurnEvent::Compacted {
                summary_chars: 10,
                upto_seq: 2,
            },
            TurnEvent::Iteration { n: 1 },
            TurnEvent::ToolCall {
                id: "c1",
                name: "fetch_video",
                args: &args,
            },
            TurnEvent::ToolResult {
                id: "c1",
                name: "fetch_video",
                is_error: false,
                external_calls: 3,
                credits: 3,
                vision_calls: 1,
                preview: "abc",
            },
            TurnEvent::Token { text: "片" },
            TurnEvent::Answer { text: "hi" },
        ];
        for ev in &evs {
            let v = event_json(ev);
            assert!(
                v.get("t").and_then(Value::as_str).is_some(),
                "缺 t 字段: {v}"
            );
        }
    }

    /// NDJSON 的命脉:**一个事件正好一行**。
    ///
    /// serde_json 默认是紧凑输出,不会带换行——但要是哪天有人给某个字段塞了
    /// 带换行的文本(工具结果预览、答案正文都可能),序列化会把它转义成 `\n`
    /// 而不是真换行。这条测试钉住这件事。
    #[test]
    fn a_multiline_payload_still_serializes_to_exactly_one_line() {
        let v = event_json(&TurnEvent::Answer {
            text: "第一行\n第二行\r\n第三行",
        });
        let s = v.to_string();
        assert_eq!(s.lines().count(), 1, "序列化后不是一行: {s}");
        assert!(s.contains("\\n"), "换行该被转义成 \\\\n: {s}");
    }

    #[test]
    fn the_sink_writes_one_line_per_event_and_flushes() {
        let mut buf = Vec::new();
        {
            let sink = NdjsonSink::new(&mut buf);
            sink.on(&TurnEvent::Iteration { n: 1 });
            sink.on(&TurnEvent::Answer { text: "完成" });
        }
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2, "{s}");
        // 每一行都要能单独解析——Go 那边就是一行一行解的
        for l in &lines {
            serde_json::from_str::<Value>(l).unwrap_or_else(|e| panic!("解析失败 {l}: {e}"));
        }
    }

    /// 签名是**输出时**拼的，所以 answer 事件里有它。
    #[test]
    fn the_answer_event_carries_the_signature() {
        let v = event_json(&TurnEvent::Answer {
            text: "结论是这样"
        });
        let t = v["text"].as_str().unwrap();
        assert!(t.starts_with("结论是这样"));
        assert!(
            t.ends_with(crate::content::evidence::ANSWER_SIGNATURE),
            "签名没拼上: {t}"
        );
        // 拼完还是一行 JSON
        assert_eq!(v.to_string().lines().count(), 1);
    }

    /// ★ 但 token 事件**不能**带签名。
    ///
    /// token 是正文的碎片，前端会把它们拼起来显示。要是每片都拼一次签名，
    /// 屏幕上会出现几百个签名；只在最后一片拼也不行——事先不知道哪片是最后。
    /// 签名只在 answer 那一次出现，前端拿它覆盖流出来的内容。
    #[test]
    fn token_events_never_carry_the_signature() {
        let v = event_json(&TurnEvent::Token { text: "结论" });
        assert_eq!(v["text"], "结论");
    }

    /// token 事件也必须是一行。正文里本来就有换行，而 token 是从正文切出来的
    /// 片——某一片正好落在换行上时，序列化必须转义，否则一片会变成两行、
    /// Go 那边解析立刻乱套。
    #[test]
    fn a_token_carrying_a_newline_still_serializes_to_one_line() {
        let v = event_json(&TurnEvent::Token {
            text: "第一行\n第二",
        });
        let s = v.to_string();
        assert_eq!(s.lines().count(), 1, "序列化后不是一行: {s}");
        assert_eq!(v["t"], "token");
    }

    /// outcome 的字面量是协议的一部分,Go 会 match 它们。
    /// 改这里就是破坏性变更,要连 PROTOCOL_VERSION 一起动。
    #[test]
    fn outcome_tags_are_frozen() {
        assert_eq!(outcome_tag(&TurnOutcome::Done), "done");
        assert_eq!(outcome_tag(&TurnOutcome::IterationCap), "iteration_cap");
        assert_eq!(outcome_tag(&TurnOutcome::Truncated), "truncated");
        assert_eq!(
            outcome_tag(&TurnOutcome::ProtocolError("x".into())),
            "protocol_error"
        );
        assert_eq!(
            outcome_tag(&TurnOutcome::ContextBudget { used: 1, limit: 2 }),
            "context_budget"
        );
        assert_eq!(
            outcome_tag(&TurnOutcome::ModelError("x".into())),
            "model_error"
        );
    }

    #[test]
    fn hello_says_null_when_no_vision_model_is_configured() {
        let v = hello_json("s1", "deepseek-chat", None);
        assert!(v["vision"].is_null());
        assert_eq!(v["protocol"], PROTOCOL_VERSION);
        let v2 = hello_json("s1", "deepseek-chat", Some("qwen3-vl-plus"));
        assert_eq!(v2["vision"], "qwen3-vl-plus");
    }
}

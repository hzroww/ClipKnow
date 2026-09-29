//! 把执行事件从**阻塞线程**送到 **async 的 SSE 响应**上。
//!
//! ```text
//! blocking 线程                             worker 线程
//!   app::turn::execute
//!       │ TurnObserver::on / TurnSink::emit_json
//!       ▼
//!   ChannelSink  ──►  有界 mpsc 队列（1024）  ──►  axum Sse  ──► Go ──► 浏览器
//!       │
//!       └─ 队列满 / 对端关了：见下面「满了怎么办」
//! ```
//!
//! ## 满了怎么办
//!
//! 设计文档第 6.2 节：「慢客户端或断开连接**不得阻塞 Agent 无限等待**，
//! 也不能造成无界事件缓冲」。所以不能用 `blocking_send`——浏览器卡住时它
//! 会把整个 agent 循环一起卡住。
//!
//! 分两类处理：
//!
//! | 事件 | 满了 | 为什么 |
//! |---|---|---|
//! | `token` / `iteration` / 工具事件 | **直接丢**，计数 +1 | token 是正文的碎片，完整正文在最后的 `answer` 里还有一份，前端本来就是用 `answer` 覆盖流式内容的 |
//! | `hello` / `usage` / `done` / `error` | 有限重试，最多 5 秒 | 丢了 `done` 的话浏览器那边永远转圈。这时候循环已经跑完了，等一下不占任何资源 |
//!
//! **有限**重试，不是无限等待：用 `try_send` 加短睡眠，攒够次数就放弃。
//! 半开的 TCP 连接（对端没了但没发 FIN）能让写操作挂上几分钟，无限等待
//! 会把一条 blocking 线程连同那个执行名额一起占住。
//!
//! 丢了多少条会拼进 `done` 事件的 note 里，不是悄悄丢。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::runner::{TurnEvent, TurnObserver};
use crate::wire::{TurnSink, event_json};

/// 队列深度。
///
/// 一次提问大概产出几百到几千个 `token` 事件。1024 的意思是「消费端落后
/// 一千条以内都不丢」——正常情况下 SSE 那边是边收边写，根本积不起来。
/// 真积满了说明对端已经基本不动了，那时候丢掉正文碎片是正确的选择。
const QUEUE_DEPTH: usize = 1024;

/// 关键事件重试的次数和间隔。乘起来 5 秒。
const IMPORTANT_RETRIES: usize = 100;
const IMPORTANT_RETRY_GAP: Duration = Duration::from_millis(50);

pub struct ChannelSink {
    /// 送出去的是**已经序列化好的一行 JSON**，不是 Value。
    /// 序列化发生在阻塞线程上（那边有的是时间），async 那边只管往外写。
    tx: mpsc::Sender<String>,
    dropped: AtomicUsize,
}

impl ChannelSink {
    /// 建一个 sink 和配套的接收端。接收端交给 SSE 响应。
    pub fn new() -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        (
            ChannelSink {
                tx,
                dropped: AtomicUsize::new(0),
            },
            rx,
        )
    }

    /// 丢了多少条。收尾时拼进 done 的 note 里。
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 可丢的事件：满了就丢，绝不等待。
    fn push_lossy(&self, line: String) {
        if self.tx.try_send(line).is_err() {
            // Full（消费端太慢）和 Closed（浏览器走了）都走这里。
            // 两种都不该让循环停下——那一轮已经花了模型 token 和 SC 配额，
            // 结果要落库。
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 关键事件：有限重试，最多等 5 秒。
    fn push_important(&self, line: String) {
        let mut pending = line;
        for _ in 0..IMPORTANT_RETRIES {
            match self.tx.try_send(pending) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(back)) => {
                    pending = back;
                    std::thread::sleep(IMPORTANT_RETRY_GAP);
                }
                // 对端已经关了，再等也没用——浏览器断了而已，执行继续。
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// 哪些事件算关键。
    ///
    /// 判据是**丢了会让对端进入一个错误的状态**：
    ///   - `done` / `error` 丢了 → 浏览器永远转圈（Go 那边有兜底会补一条
    ///     「异常退出」，但那句话是错的，这一轮其实成功了）
    ///   - `hello` 丢了 → 前端拿不到新会话的 id，下一轮又新建一个会话
    ///   - `usage` 丢了 → 只是少显示一行用量，但它紧挨着 done，
    ///     一起重试代价为零
    fn is_important(v: &Value) -> bool {
        matches!(
            v.get("t").and_then(Value::as_str),
            Some("hello" | "usage" | "done" | "error")
        )
    }
}

impl TurnObserver for ChannelSink {
    fn on(&self, ev: &TurnEvent<'_>) {
        // 循环内部的事件全是可丢的：token 是正文碎片，工具事件是进度显示。
        self.push_lossy(event_json(ev).to_string());
    }
}

impl TurnSink for ChannelSink {
    fn emit_json(&self, v: &Value) {
        if !Self::is_important(v) {
            self.push_lossy(v.to_string());
            return;
        }
        // ★ 丢了多少条要说出来，不能悄悄丢。
        //
        //   拼进 done 的 note 里而不是单发一个事件：Go 那边靠**最后一条**
        //   是不是 done/error 判断流有没有正常收尾（chat.go 的
        //   endedProperly），在 done 后面再加东西会把那个判断搞坏。
        self.push_important(with_drop_note(v, self.dropped()).to_string());
    }
}

/// 给 done 事件的 note 补一句「丢了几条进度」。不是 done、或者一条没丢时原样返回。
fn with_drop_note(v: &Value, dropped: usize) -> std::borrow::Cow<'_, Value> {
    if dropped == 0 || v.get("t").and_then(Value::as_str) != Some("done") {
        return std::borrow::Cow::Borrowed(v);
    }
    let mut out = v.clone();
    let note = out
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let extra = format!("（网络跟不上，{dropped} 条进度没显示出来；答案本身是完整的）");
    out["note"] = Value::String(if note.is_empty() {
        extra
    } else {
        format!("{note}{extra}")
    });
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 事件原样变成一行_json() {
        let (sink, mut rx) = ChannelSink::new();
        sink.on(&TurnEvent::Iteration { n: 3 });
        let line = rx.try_recv().unwrap();
        assert_eq!(line.lines().count(), 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["t"], "iteration");
        assert_eq!(v["n"], 3);
    }

    /// ★ 队列满了**不能阻塞**，这是这个文件存在的全部理由。
    ///
    /// 阻塞的后果是浏览器一卡，整个 agent 循环跟着停——而那一轮已经花了
    /// 模型 token 和 SC 配额。
    #[test]
    fn 队列满了就丢_不阻塞() {
        let (sink, _rx) = ChannelSink::new();
        let t0 = std::time::Instant::now();
        // 灌 QUEUE_DEPTH + 500 条，后面 500 条必然满
        for n in 0..QUEUE_DEPTH + 500 {
            sink.on(&TurnEvent::Iteration { n });
        }
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "灌满队列花了 {:?}，说明在等——这会把 agent 循环一起卡住",
            t0.elapsed()
        );
        assert_eq!(sink.dropped(), 500, "丢的条数没对上");
    }

    /// 对端走了（浏览器断开）之后继续发，不能 panic。
    #[test]
    fn 接收端没了之后继续发也不会炸() {
        let (sink, rx) = ChannelSink::new();
        drop(rx);
        sink.on(&TurnEvent::Iteration { n: 1 });
        sink.emit_json(&json!({"t": "done", "outcome": "done", "note": ""}));
        assert_eq!(sink.dropped(), 2);
    }

    /// ★ done 这类关键事件，队列暂时满的时候要等出空位，不能直接丢。
    ///
    /// 丢了 done 的表现是浏览器永远转圈——而那一轮其实已经成功落库了。
    #[test]
    fn 队列暂时满时_done_会等到有空位() {
        let (sink, mut rx) = ChannelSink::new();
        for n in 0..QUEUE_DEPTH {
            sink.on(&TurnEvent::Iteration { n });
        }
        assert_eq!(sink.dropped(), 0, "刚好装满，不该丢");

        // 150 毫秒后腾出一个位置
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let _ = rx.blocking_recv();
            rx // 还给主线程，别让它提前 drop
        });
        sink.emit_json(&json!({"t": "done", "outcome": "done", "note": ""}));
        let _rx = h.join().unwrap();
        assert_eq!(sink.dropped(), 0, "done 被丢掉了");
    }

    /// 关键事件也不能无限等：对端一直不收，攒够次数就放弃。
    ///
    /// 不放弃的话，半开的 TCP 连接会把一条 blocking 线程连同那个执行名额
    /// 一起占住。
    #[test]
    fn 关键事件等不到空位最终也会放弃() {
        let (sink, _rx) = ChannelSink::new();
        for n in 0..QUEUE_DEPTH {
            sink.on(&TurnEvent::Iteration { n });
        }
        let t0 = std::time::Instant::now();
        sink.emit_json(&json!({"t": "done", "outcome": "done", "note": ""}));
        let waited = t0.elapsed();
        assert_eq!(sink.dropped(), 1, "该放弃了却没放弃");
        assert!(
            waited >= Duration::from_secs(1) && waited < Duration::from_secs(15),
            "等了 {waited:?}，期望在 5 秒上下"
        );
    }

    #[test]
    fn 丢了进度会写进_done_的_note_里() {
        let (sink, mut rx) = ChannelSink::new();
        for n in 0..QUEUE_DEPTH + 3 {
            sink.on(&TurnEvent::Iteration { n });
        }
        // 腾出位置让 done 发得出去
        for _ in 0..10 {
            let _ = rx.blocking_recv();
        }
        sink.emit_json(&json!({"t": "done", "outcome": "done", "note": "原本的话。"}));

        let mut last = String::new();
        while let Ok(l) = rx.try_recv() {
            last = l;
        }
        let v: Value = serde_json::from_str(&last).unwrap();
        assert_eq!(v["t"], "done", "最后一条必须还是 done：{last}");
        let note = v["note"].as_str().unwrap();
        assert!(
            note.starts_with("原本的话。"),
            "原来的 note 被盖掉了：{note}"
        );
        assert!(note.contains("3 条"), "没说丢了几条：{note}");
    }

    #[test]
    fn 一条都没丢时_done_的_note_原样不动() {
        let v = json!({"t": "done", "outcome": "done", "note": "原本的话。"});
        assert_eq!(with_drop_note(&v, 0).as_ref(), &v);
    }

    #[test]
    fn 哪些事件算关键是按_t_字段判的() {
        for t in ["hello", "usage", "done", "error"] {
            assert!(ChannelSink::is_important(&json!({"t": t})), "{t} 该算关键");
        }
        for t in [
            "token",
            "iteration",
            "tool_call",
            "tool_result",
            "answer",
            "compacted",
        ] {
            assert!(
                !ChannelSink::is_important(&json!({"t": t})),
                "{t} 不该算关键"
            );
        }
    }
}

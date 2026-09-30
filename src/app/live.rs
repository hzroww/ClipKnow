//! 一次正在跑的提问的**事件记录**：发过哪些事件，按顺序全留着。
//!
//! ## 为什么要留
//!
//! 为了**刷新页面之后接着看**。改造前，事件是边产生边发给那一个浏览器
//! 连接的，发完就没了。刷新 = 连接断了 = 后面的 token 一个都看不到，
//! 前面已经显示的半截也跟着消失（token 从来不写库，库里只存最终结果）。
//!
//! 现在每一轮在跑的提问都有一份这样的记录。任何时候来一个新的订阅者
//! （刷新后的页面、另一个标签页），都**从第一条开始补发**，补完了接着
//! 实时推。原来那个发起提问的连接，也只是其中一个订阅者。
//!
//! 这件事只有在 Rust 常驻之后才做得了：以前一问一个子进程、答完就退，
//! 这份记录没有地方放。
//!
//! ## 两条规矩
//!
//! 1. **写入永远不等任何人。** agent 循环往这里追加一条，只是在一把锁里
//!    push 一下，毫秒都不到。慢的订阅者只会让**它自己的**转发任务等着
//!    （见 `serve::subscribe_sse`），不会拖慢循环，也不会拖慢别的订阅者。
//!    设计文档第 6.2 节：慢客户端不得阻塞 Agent。
//!
//! 2. **有上限。** 一轮提问通常几百到几千个事件，每个几十字节。上限
//!    [`MAX_EVENTS`] 条，超了就不再记 token（正文碎片，完整正文在最后的
//!    `answer` 里还有一份），但关键事件（hello / 工具 / answer / done /
//!    error）照记。丢了多少会写进 done 的 note 里。

use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tokio::sync::watch;

use crate::agent::runner::{TurnEvent, TurnObserver};
use crate::wire::{TurnSink, event_json};

/// 一轮最多记多少条可丢的事件（主要是 token）。
///
/// 实测一条 1066 字的答案分成 655 片；几轮工具调用加起来也就几千条。
/// 两万条是十倍余量，内存上限大约几 MB。
pub const MAX_EVENTS: usize = 20_000;

#[derive(Default)]
struct Log {
    /// 已经序列化好的 JSON，一条一行。`Arc<str>` 是为了多个订阅者读同一条
    /// 时不用各拷一份。
    events: Vec<Arc<str>>,
    /// 这一轮结束了（done / error 已经发出，或者执行任务退出了）。
    finished: bool,
    /// 因为超过上限没记下来的条数。
    dropped: usize,
}

pub struct LiveTurn {
    log: Mutex<Log>,
    /// 每追加一条就 +1。订阅者靠它知道「有新东西了」。
    ///
    /// 用 watch 而不是 Notify：watch 记得「版本号」，订阅者只要在读记录
    /// **之前**订阅，就不可能漏掉在「读完」和「开始等」之间发生的那次更新。
    /// Notify 在那个空隙里发的通知会丢，订阅者就永远卡在等待上。
    tick: watch::Sender<u64>,
}

impl LiveTurn {
    pub fn new() -> Arc<Self> {
        let (tick, _) = watch::channel(0);
        Arc::new(LiveTurn {
            log: Mutex::new(Log::default()),
            tick,
        })
    }

    /// 中毒了照常用：这把锁保护的只是一个 Vec，push 是单条语句，
    /// 不会留下半改完的状态。
    fn lock(&self) -> MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        // send_modify 没有订阅者也照样生效（send 在没人订阅时会报错）。
        self.tick.send_modify(|v| *v += 1);
    }

    fn push(&self, line: String, important: bool) {
        {
            let mut log = self.lock();
            if log.finished {
                // 收尾之后还来的（理论上没有）不记，免得订阅者已经走了
                // 却还有东西进来。
                return;
            }
            if !important && log.events.len() >= MAX_EVENTS {
                log.dropped += 1;
                return;
            }
            log.events.push(Arc::from(line));
        }
        self.bump();
    }

    /// 这一轮结束了。订阅者读完剩下的就会退出。
    ///
    /// 可以调多次（执行任务正常收尾调一次，名额归还时再兜底调一次）。
    pub fn finish(&self) {
        {
            let mut log = self.lock();
            if log.finished {
                return;
            }
            log.finished = true;
        }
        self.bump();
    }

    /// 从第 `cursor` 条开始读，返回 (新的这些条, 是否已结束)。
    pub fn read_from(&self, cursor: usize) -> (Vec<Arc<str>>, bool) {
        let log = self.lock();
        let batch = log.events.get(cursor..).unwrap_or_default().to_vec();
        (batch, log.finished)
    }

    /// 订阅「有新东西了」的信号。**必须在第一次 read_from 之前调**。
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.tick.subscribe()
    }

    pub fn dropped(&self) -> usize {
        self.lock().dropped
    }

    /// 丢了会让对端进入错误状态的事件，一律照记：
    ///   hello        丢了前端拿不到新会话的 id
    ///   answer       完整正文，token 丢了全靠它补
    ///   done / error 丢了浏览器永远转圈
    ///   usage        紧挨着 done，一起记代价为零
    ///   工具 / 轮次  进度区靠它们重建；数量少，不占地方
    fn is_important(v: &Value) -> bool {
        !matches!(v.get("t").and_then(Value::as_str), Some("token"))
    }
}

impl TurnObserver for LiveTurn {
    fn on(&self, ev: &TurnEvent<'_>) {
        let v = event_json(ev);
        let important = Self::is_important(&v);
        self.push(v.to_string(), important);
    }
}

impl TurnSink for LiveTurn {
    fn emit_json(&self, v: &Value) {
        let v = with_drop_note(v, self.dropped());
        self.push(v.to_string(), Self::is_important(&v));
    }
}

/// 给 done 事件的 note 补一句「丢了几条进度」。不是 done、或者一条没丢时原样返回。
///
/// 拼进 done 里而不是单发一个事件：Go 那边靠**最后一条**是不是 done/error
/// 判断流有没有正常收尾，在 done 后面再加东西会把那个判断搞坏。
fn with_drop_note(v: &Value, dropped: usize) -> Cow<'_, Value> {
    if dropped == 0 || v.get("t").and_then(Value::as_str) != Some("done") {
        return Cow::Borrowed(v);
    }
    let mut out = v.clone();
    let note = out
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let extra = format!("（这一轮流式片段太多，{dropped} 片没留存；答案本身是完整的）");
    out["note"] = Value::String(if note.is_empty() {
        extra
    } else {
        format!("{note}{extra}")
    });
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(batch: &[Arc<str>]) -> Vec<Value> {
        batch
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn 按顺序记下每一条() {
        let live = LiveTurn::new();
        live.on(&TurnEvent::Iteration { n: 1 });
        live.on(&TurnEvent::Token { text: "你" });
        live.on(&TurnEvent::Token { text: "好" });
        live.emit_json(&json!({"t": "done", "outcome": "done", "note": ""}));
        let (batch, finished) = live.read_from(0);
        let ts: Vec<_> = texts(&batch).iter().map(|v| v["t"].clone()).collect();
        assert_eq!(ts, vec!["iteration", "token", "token", "done"]);
        assert!(!finished, "没调 finish 之前不该算结束");
    }

    /// ★ 后来的订阅者从头补发——这就是「刷新之后接着看」。
    #[test]
    fn 从任意位置接着读() {
        let live = LiveTurn::new();
        for n in 0..5 {
            live.on(&TurnEvent::Iteration { n });
        }
        let (first, _) = live.read_from(0);
        assert_eq!(first.len(), 5);
        let (rest, _) = live.read_from(3);
        assert_eq!(texts(&rest)[0]["n"], 3);
        let (none, _) = live.read_from(99);
        assert!(none.is_empty(), "越界不该 panic");
    }

    /// ★ 写入永远不等人。
    ///
    /// 灌远超上限的 token，必须瞬间完成——它跑在 agent 循环里，慢一点就是
    /// 整个提问慢一点。
    #[test]
    fn 写入不等人_超过上限只丢_token() {
        let live = LiveTurn::new();
        let t0 = std::time::Instant::now();
        for _ in 0..MAX_EVENTS + 500 {
            live.on(&TurnEvent::Token { text: "片" });
        }
        live.on(&TurnEvent::Iteration { n: 2 });
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(live.dropped(), 500);
        let (all, _) = live.read_from(0);
        assert_eq!(all.len(), MAX_EVENTS + 1, "超上限后的轮次事件也该照记");
    }

    #[test]
    fn 丢了片段会写进_done_的_note_且_done_仍是最后一条() {
        let live = LiveTurn::new();
        for _ in 0..MAX_EVENTS + 3 {
            live.on(&TurnEvent::Token { text: "片" });
        }
        live.emit_json(&json!({"t": "done", "outcome": "done", "note": "原本的话。"}));
        let (all, _) = live.read_from(0);
        let last: Value = serde_json::from_str(all.last().unwrap()).unwrap();
        assert_eq!(last["t"], "done");
        let note = last["note"].as_str().unwrap();
        assert!(note.starts_with("原本的话。"), "{note}");
        assert!(note.contains("3 片"), "{note}");
    }

    #[test]
    fn 收尾可以调多次_收尾之后不再记() {
        let live = LiveTurn::new();
        live.on(&TurnEvent::Iteration { n: 1 });
        live.finish();
        live.finish();
        live.on(&TurnEvent::Iteration { n: 2 });
        let (all, finished) = live.read_from(0);
        assert!(finished);
        assert_eq!(all.len(), 1);
    }

    /// ★ 订阅者不会漏掉「读完」和「开始等」之间来的那一条。
    ///
    /// 这是选 watch 不选 Notify 的全部理由。顺序：先订阅，再读，读完之后
    /// 才来一条新的——changed() 必须能立刻醒。
    #[test]
    fn 先订阅再读_之后来的一条不会漏() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let live = LiveTurn::new();
        let mut rx = live.watch();
        let (first, _) = live.read_from(0);
        assert!(first.is_empty());
        live.on(&TurnEvent::Iteration { n: 1 }); // 读完之后才来
        let woke = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.changed()).await
        });
        assert!(matches!(woke, Ok(Ok(()))), "漏掉了读完之后来的那一条");
    }
}

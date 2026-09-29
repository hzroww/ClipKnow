//! 执行准入登记表：**现在有哪几次提问在跑**。
//!
//! ## 它替代了什么
//!
//! 以前这件事是 Go 那边一把内存锁（`web/chat.go` 的 `turnGate`）：同一时刻
//! 只让一个提问跑，抢不到就立刻 409。搬到 Rust 是因为设计文档第 12 节的第
//! 4 条不变量——**只有一个执行入口定义准入规则，网页和 CLI 不各写一套**。
//! 留在 Go 那边的话，命令行绕过 web 直接跑就不受任何约束。
//!
//! ## 阶段 B 的策略：全局最多 1 个
//!
//! 和改造前**完全一样**的行为。这一步只搬机制，不放宽策略——放宽是阶段 C
//! 的事，那时候要先有会话互斥的数据库兜底、取消、崩溃恢复。
//!
//! 阶段 C 要改的只有两处：`max_total` 这个数字，和加一张
//! `per_user: HashMap<user_id, usize>`。数据结构和调用点都不用动。
//!
//! ## 为什么名额是个 guard 而不是一对 acquire/release
//!
//! [`Permit`] 的 `Drop` 归还名额。正常返回、`?` 提前返回、甚至 panic，
//! 都会走 `Drop`。设计文档第 7.1 节要求「全部退出路径释放容量」——写成
//! guard 之后这件事由类型系统保证，不靠我记得在每个分支写一句归还。
//!
//! 少写一次归还的后果是**服务永久性地不再接受任何提问**，而且不报任何错，
//! 看起来就是"卡住了"。这类 bug 特别难查，所以值得用类型来挡。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 阶段 B 的全局并发上限。**保持改造前的行为：一次只跑一个。**
///
/// 这个数字就是阶段 C 要动的地方。动之前必须先有：会话互斥的数据库兜底
/// （turns 表的部分唯一索引）、取消、启动时的崩溃恢复。
pub const MAX_CONCURRENT_TURNS: usize = 1;

/// 没拿到名额的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    /// 这个会话已经有一次提问在跑。带上跑了多久，好拼一句有信息量的话。
    SessionBusy { elapsed: Duration },
    /// 全局名额满了（跑的是**别的**会话）。
    Capacity { running: usize, elapsed: Duration },
    /// 正在停机，不再接受新的。
    ShuttingDown,
}

impl Reject {
    /// 给人看的一句话。
    pub fn message(&self) -> String {
        match self {
            Reject::SessionBusy { elapsed } => format!(
                "这个会话上一个问题还在跑（已经 {:.0} 秒），等它结束再问",
                elapsed.as_secs_f64()
            ),
            Reject::Capacity { running, elapsed } => format!(
                "服务正忙（{running} 个提问在跑，最久的已经 {:.0} 秒），稍后再试",
                elapsed.as_secs_f64()
            ),
            Reject::ShuttingDown => "服务正在停机，暂时不接受新的提问".into(),
        }
    }

    /// 稳定的错误码。Go 按它决定给浏览器什么状态。
    pub fn code(&self) -> &'static str {
        match self {
            Reject::SessionBusy { .. } => "session_busy",
            Reject::Capacity { .. } => "capacity",
            Reject::ShuttingDown => "shutting_down",
        }
    }
}

#[derive(Default)]
struct Inner {
    /// session_id → 这次执行是什么时候开始的
    running: HashMap<String, Instant>,
    /// 收到停机信号之后变 false，之后一律拒绝。
    accepting: bool,
}

pub struct Registry {
    inner: Mutex<Inner>,
    max_total: usize,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(MAX_CONCURRENT_TURNS)
    }

    /// 指定上限。测试用它造出「多个会话能同时跑」的场景，
    /// 顺便让阶段 C 改上限时不用动结构。
    pub fn with_capacity(max_total: usize) -> Arc<Self> {
        Arc::new(Registry {
            inner: Mutex::new(Inner {
                running: HashMap::new(),
                accepting: true,
            }),
            max_total,
        })
    }

    /// 中毒了也照常继续。中毒的意思是「上一个持有者 panic 了」——而这把锁
    /// 保护的只是一张 HashMap，panic 不会把它留在半改完的状态（插入和删除
    /// 都是单条语句）。默认行为（panic）会让**一次** panic 永久废掉整个
    /// 服务的准入，后果比原来的 bug 严重得多。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 申请跑一次提问。拿到 [`Permit`] 才能开跑。
    ///
    /// 两项检查在**同一把锁里**完成，中间没有空窗：
    ///   ① 这个会话是不是已经在跑（→ SessionBusy）
    ///   ② 全局名额够不够（→ Capacity）
    ///
    /// 抢不到**不排队**。排队的话用户点了发送、界面一动不动，分不清是卡了
    /// 还是坏了；明确拒绝并说明原因，至少知道发生了什么。
    pub fn admit(self: &Arc<Self>, session_id: &str) -> Result<Permit, Reject> {
        let mut inner = self.lock();
        if !inner.accepting {
            return Err(Reject::ShuttingDown);
        }
        if let Some(started) = inner.running.get(session_id) {
            return Err(Reject::SessionBusy {
                elapsed: started.elapsed(),
            });
        }
        if inner.running.len() >= self.max_total {
            let oldest = inner
                .running
                .values()
                .map(|t| t.elapsed())
                .max()
                .unwrap_or_default();
            return Err(Reject::Capacity {
                running: inner.running.len(),
                elapsed: oldest,
            });
        }
        inner.running.insert(session_id.to_string(), Instant::now());
        Ok(Permit {
            registry: Arc::clone(self),
            session_id: session_id.to_string(),
        })
    }

    /// 现在有几个在跑。`/internal/health` 用它。
    pub fn running_count(&self) -> usize {
        self.lock().running.len()
    }

    pub fn is_accepting(&self) -> bool {
        self.lock().accepting
    }

    /// 停机第一步：不再接受新的提问，已经在跑的不动。
    pub fn stop_accepting(&self) {
        self.lock().accepting = false;
    }

    /// 停机第二步：等在跑的都结束，最多等 `timeout`。
    ///
    /// 返回还剩几个没跑完（0 = 都收干净了）。
    ///
    /// 轮询而不是用条件变量：停机一辈子就走一次，多等 100 毫秒无所谓，
    /// 而条件变量要求每条归还路径都记得 notify——又回到了「靠记得」。
    /// `Permit` 的 Drop 只做一件事（从 map 里删掉），这是刻意的。
    pub fn wait_until_idle(&self, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let n = self.running_count();
            if n == 0 || Instant::now() >= deadline {
                return n;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 一个名额。活着的时候占着位置，`Drop` 时归还。
///
/// **不要 `std::mem::forget` 它**，那会让那个会话永远显示为「在跑」。
///
/// Debug 手写：Registry 里有锁，derive 出来的会在打印时去拿锁，而
/// `{:?}` 经常出现在**已经持着锁**的 panic 消息里——那就是死锁。
pub struct Permit {
    registry: Arc<Registry>,
    session_id: String,
}

impl Permit {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Permit({})", self.session_id)
    }
}

impl PartialEq for Permit {
    /// 只比会话 id。有这个是为了让测试能写
    /// `assert_eq!(reg.admit(..), Err(..))`——两个 Permit 相不相等本身
    /// 没有业务含义。
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
    }
}
impl Eq for Permit {}

impl Drop for Permit {
    fn drop(&mut self) {
        self.registry.lock().running.remove(&self.session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拿到名额之后同一个会话再申请会被拒() {
        let reg = Registry::with_capacity(4);
        let _p = reg.admit("s1").unwrap();
        match reg.admit("s1") {
            Err(Reject::SessionBusy { .. }) => {}
            other => panic!("期望 SessionBusy，实际 {other:?}"),
        }
    }

    #[test]
    fn 名额还回去之后同一个会话能再申请() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1").unwrap();
        drop(p);
        // ★ 必须绑住这个 Permit。写成 `assert!(reg.admit("s1").is_ok())`
        //   的话，Permit 是个临时值，语句结束就被 Drop 掉了，下一句
        //   running_count 读到 0——第一版就是这么假红的。
        //   （顺带说明 guard 真的在工作。）
        let _again = reg.admit("s1").unwrap();
        assert_eq!(reg.running_count(), 1);
    }

    #[test]
    fn 上限是几就只能同时跑几个() {
        let reg = Registry::with_capacity(2);
        let _a = reg.admit("s1").unwrap();
        let _b = reg.admit("s2").unwrap();
        match reg.admit("s3") {
            Err(Reject::Capacity { running, .. }) => assert_eq!(running, 2),
            other => panic!("期望 Capacity，实际 {other:?}"),
        }
    }

    /// 阶段 B 的策略就是改造前那个全局串行锁：一次只跑一个。
    /// 这条钉住默认值，免得哪天有人"顺手"把它调大而没做阶段 C 的功课。
    #[test]
    fn 默认上限是一() {
        assert_eq!(MAX_CONCURRENT_TURNS, 1);
        let reg = Registry::new();
        let _a = reg.admit("s1").unwrap();
        assert!(matches!(reg.admit("s2"), Err(Reject::Capacity { .. })));
    }

    /// ★ 执行 panic 了，名额也必须还回去。
    ///
    /// 这是把名额做成 guard 的全部理由。少还一次的后果是**服务永久不再接受
    /// 任何提问**，而且不报任何错，看起来就是"卡住了"。
    #[test]
    fn 执行_panic_了名额也会还回去() {
        let reg = Registry::with_capacity(1);
        let r2 = Arc::clone(&reg);
        let boom = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _p = r2.admit("s1").unwrap();
            panic!("假装跑挂了");
        }));
        assert!(boom.is_err(), "这个闭包本该 panic");
        assert_eq!(reg.running_count(), 0, "panic 之后名额没还回来");
        assert!(reg.admit("s1").is_ok());
    }

    #[test]
    fn 停机之后一律拒绝_但在跑的不受影响() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1").unwrap();
        reg.stop_accepting();
        assert_eq!(reg.admit("s2"), Err(Reject::ShuttingDown));
        // 已经拿到名额的那个还活着
        assert_eq!(reg.running_count(), 1);
        drop(p);
        assert_eq!(reg.running_count(), 0);
    }

    #[test]
    fn 等到空闲_都结束了就立刻返回() {
        let reg = Registry::with_capacity(4);
        let t0 = Instant::now();
        assert_eq!(reg.wait_until_idle(Duration::from_secs(5)), 0);
        assert!(t0.elapsed() < Duration::from_millis(500), "空闲时不该真等");
    }

    #[test]
    fn 等到空闲_超时了会报还剩几个() {
        let reg = Registry::with_capacity(4);
        let _p = reg.admit("s1").unwrap();
        assert_eq!(reg.wait_until_idle(Duration::from_millis(200)), 1);
    }

    #[test]
    fn 等到空闲_别的线程结束之后就返回() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1").unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(p);
        });
        assert_eq!(reg.wait_until_idle(Duration::from_secs(5)), 0);
    }

    /// 多个线程同时抢同一个会话，只能有一个拿到。
    ///
    /// 「两项检查在同一把锁里」如果写成了「先查再插」两段，这条会红。
    #[test]
    fn 同一个会话被并发申请时只有一个能拿到() {
        let reg = Registry::with_capacity(8);
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut hs = Vec::new();
        for _ in 0..16 {
            let r = Arc::clone(&reg);
            let w = Arc::clone(&winners);
            hs.push(std::thread::spawn(move || {
                // 拿到的**不释放**（存进 vec 里返回），这样"同时"才成立
                match r.admit("same-session") {
                    Ok(p) => {
                        w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        Some(p)
                    }
                    Err(_) => None,
                }
            }));
        }
        let permits: Vec<_> = hs.into_iter().filter_map(|h| h.join().unwrap()).collect();
        assert_eq!(
            winners.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "同一个会话被 {} 个线程同时拿到了名额",
            permits.len()
        );
    }
}

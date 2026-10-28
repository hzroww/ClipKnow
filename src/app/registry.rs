//! 执行准入登记表：**现在有哪几次提问在跑**。
//!
//! ## 它替代了什么
//!
//! 以前这件事是 Go 那边一把内存锁（`web/chat.go` 的 `turnGate`）：同一时刻
//! 只让一个提问跑，抢不到就立刻 409。搬到 Rust 是因为设计文档第 12 节的第
//! 4 条不变量——**只有一个执行入口定义准入规则，网页和 CLI 不各写一套**。
//! 留在 Go 那边的话，命令行绕过 web 直接跑就不受任何约束。
//!
//! ## 策略
//!
//! ```text
//! 同一个会话    最多 1 个在跑      第二个 → 409「上一个问题还在跑」
//! 全局          最多 max_total 个  满了   → 503「服务正忙」
//! 每个用户      不限               （见下）
//! ```
//!
//! `max_total` 默认 [`DEFAULT_MAX_TURNS`]，`clipknow serve --max-turns N` 可改。
//!
//! ## 会话锁和全局名额是两样东西
//!
//! ```text
//! 会话锁    这一轮一结束（包括被停止）就放开，用户能马上接着问
//! 全局名额  这一轮**和它丢下的工具线程**都结束了才还
//! ```
//!
//! 用户点停止时，循环不再等正在跑的工具（`agent::exec`），但那个工具还在
//! 后台下载视频、占着内存。名额要是跟着会话锁一起还，用户反复「问、停、
//! 再问」就能叠出任意多个同时进行的下载——而「最多同时跑几个」本来就是
//! 内存的上限（一个视频整个读进内存，最大 512MB）。
//!
//! ## 取消按「这一轮」找，不按会话找
//!
//! 每一轮在申请名额时就分到一个编号（`turn_id`，之后就是 `turns` 表那一行
//! 的 id）。取消请求带着编号来，和登记表里这个会话正在跑的那一轮对不上就
//! 什么都不做——一个迟到的取消请求永远停不掉用户后来发的新问题。
//!
//! **没有「每个用户最多几个」。** 所以同一个人开两个会话可以同时问。要加的话
//! 是一张 `per_user: HashMap<user_id, usize>`，在 `admit` 里多检查一项——
//! 数据结构和调用点都不用动。
//!
//! ## 放开之前修掉的那个坑
//!
//! 一开始这里是 1（和改造前 Go 里那把锁一样）。改大之前先写了并发落库的
//! 测试，结果 180 次落库失败了 159~174 次，全是 `database is locked`——
//! 读后写的事务用了 SQLite 默认的 DEFERRED 模式。见 `store::sqlite::write_tx`。
//! 只改这个数字的话，两个提问差不多同时答完时，后落库的那个几乎必定白跑。
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

use super::live::LiveTurn;
use super::turn::TurnControl;
use crate::agent::cancel::CancelSignal;
use crate::content::model::new_id;

/// 默认最多同时跑几个提问（全局）。`clipknow serve --max-turns` 可改。
///
/// 4 不是算出来的，是「够测并发、又不至于一不小心把内存吃光」的折中：
/// 分析视频时整个文件读进内存，单个最大 512MB（`ingest::download`）。
pub const DEFAULT_MAX_TURNS: usize = 4;

/// 没拿到名额的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    /// 这个会话已经有一次提问在跑。带上跑了多久，好拼一句有信息量的话。
    SessionBusy { elapsed: Duration },
    /// 全局名额满了（跑的是**别的**会话）。
    Capacity { running: usize, elapsed: Duration },
    /// 正在停机，不再接受新的。
    ShuttingDown,
    /// 这个会话正在跑的，就是**同一次提交**（同一个 request_id）跑起来的那一轮。
    /// 不是拒绝：是重试撞上了自己，告诉调用方「就是这一轮」。
    ///
    /// 只有这里认得出这种情况：一轮从拿到名额到写进 turns 表之间有一小段空档，
    /// 那时 chat_requests 里的 turn_id 还是空的，查库认不出来。
    Duplicate { turn_id: String },
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
                "服务正忙（{running} 个提问在跑或刚停止还在收尾，最久的已经 {:.0} 秒），稍后再试",
                elapsed.as_secs_f64()
            ),
            Reject::ShuttingDown => "服务正在停机，暂时不接受新的提问".into(),
            Reject::Duplicate { .. } => "这次提交已经在跑了".into(),
        }
    }

    /// 稳定的错误码。Go 按它决定给浏览器什么状态。
    pub fn code(&self) -> &'static str {
        match self {
            Reject::SessionBusy { .. } => "session_busy",
            Reject::Capacity { .. } => "capacity",
            Reject::ShuttingDown => "shutting_down",
            Reject::Duplicate { .. } => "duplicate",
        }
    }
}

/// 一个正在跑的执行。
struct Running {
    started: Instant,
    /// 这一轮的编号。取消请求拿它核对目标。
    turn_id: String,
    /// 这一轮来自哪次提交（浏览器生成的 request_id）。命令行没有。
    request_id: Option<String>,
    /// 这一轮的取消开关。
    cancel: CancelSignal,
    /// 这一轮发过的全部事件。刷新后的页面从这里补发、接着看。
    live: Arc<LiveTurn>,
}

#[derive(Default)]
struct Inner {
    /// session_id → 正在跑的那一轮（这就是「会话锁」）
    running: HashMap<String, Running>,
    /// 全局名额用掉了几个。**可能比 `running` 多**：被停止的轮次丢下的工具
    /// 线程还在跑时，它的名额还没还。
    slots_in_use: usize,
    /// 收到停机信号之后变 false，之后一律拒绝。
    accepting: bool,
}

pub struct Registry {
    inner: Mutex<Inner>,
    max_total: usize,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(DEFAULT_MAX_TURNS)
    }

    /// 全局上限是多少。`/internal/health` 报给外面看。
    pub fn max_total(&self) -> usize {
        self.max_total
    }

    /// 指定上限。测试用它造出「多个会话能同时跑」的场景，
    /// 顺便让阶段 C 改上限时不用动结构。
    pub fn with_capacity(max_total: usize) -> Arc<Self> {
        Arc::new(Registry {
            inner: Mutex::new(Inner {
                running: HashMap::new(),
                slots_in_use: 0,
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
    ///   ② 全局名额够不够（→ Capacity）。数的是名额，不是在跑的轮次——
    ///      被停止的轮次丢下的工具还在跑时也占着一个
    ///
    /// 抢不到**不排队**。排队的话用户点了发送、界面一动不动，分不清是卡了
    /// 还是坏了；明确拒绝并说明原因，至少知道发生了什么。
    ///
    /// `request_id`：这一轮来自哪次提交。会话正被占着、而占着它的就是同一次
    /// 提交时，返回 [`Reject::Duplicate`] 而不是「会话忙」——那是重试撞上了自己。
    pub fn admit(
        self: &Arc<Self>,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Result<Permit, Reject> {
        let mut inner = self.lock();
        if !inner.accepting {
            return Err(Reject::ShuttingDown);
        }
        if let Some(r) = inner.running.get(session_id) {
            if request_id.is_some() && r.request_id.as_deref() == request_id {
                return Err(Reject::Duplicate {
                    turn_id: r.turn_id.clone(),
                });
            }
            return Err(Reject::SessionBusy {
                elapsed: r.started.elapsed(),
            });
        }
        if inner.slots_in_use >= self.max_total {
            let oldest = inner
                .running
                .values()
                .map(|r| r.started.elapsed())
                .max()
                .unwrap_or_default();
            return Err(Reject::Capacity {
                running: inner.slots_in_use,
                elapsed: oldest,
            });
        }
        let live = LiveTurn::new();
        let turn_id = new_id();
        let cancel = CancelSignal::new();
        inner.running.insert(
            session_id.to_string(),
            Running {
                started: Instant::now(),
                turn_id: turn_id.clone(),
                request_id: request_id.map(str::to_string),
                cancel: cancel.clone(),
                live: Arc::clone(&live),
            },
        );
        inner.slots_in_use += 1;
        Ok(Permit {
            registry: Arc::clone(self),
            session_id: session_id.to_string(),
            turn_id,
            cancel,
            live,
            slot: Arc::new(Slot {
                registry: Arc::clone(self),
            }),
        })
    }

    /// 停止一轮：会话对、编号也对，才打开它的取消开关。
    ///
    /// 返回它的事件记录，调用方用它等这一轮真正结束（记录被标成结束的那一刻，
    /// 会话锁已经放开了，见 `Permit` 的 Drop）。
    ///
    /// 返回 None：这个会话现在没在跑，或者跑的不是这一轮——**什么都不做**。
    /// 这一轮多半已经结束了，调用方去库里查它的结局。
    pub fn request_cancel(&self, session_id: &str, turn_id: &str) -> Option<Arc<LiveTurn>> {
        let inner = self.lock();
        let r = inner.running.get(session_id)?;
        if r.turn_id != turn_id {
            return None;
        }
        r.cancel.cancel();
        Some(Arc::clone(&r.live))
    }

    /// 全局名额用掉了几个（含被停止的轮次还在后台跑的工具）。
    pub fn slots_in_use(&self) -> usize {
        self.lock().slots_in_use
    }

    /// 这个会话正在跑的那一轮的事件记录。没在跑返回 None。
    ///
    /// 刷新后的页面靠它「接着看」：拿到之后从第一条开始补发。
    pub fn live(&self, session_id: &str) -> Option<Arc<LiveTurn>> {
        self.lock()
            .running
            .get(session_id)
            .map(|r| Arc::clone(&r.live))
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
    /// 只等**轮次**，不等被停止的轮次丢下的工具线程：它们的结果本来就作废，
    /// 进程退出时一起结束。它们正在写的缓存是一个 SQLite 事务，写一半断掉
    /// 等于没写。
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
    turn_id: String,
    cancel: CancelSignal,
    live: Arc<LiveTurn>,
    /// 全局名额。Permit 手里一份，工具线程手里各一份，最后一份放手时才还。
    slot: Arc<Slot>,
}

impl Permit {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// 跑这一轮要带上的：编号、取消开关、全局名额。交给 `app::turn::execute`。
    pub fn control(&self) -> TurnControl {
        TurnControl {
            turn_id: self.turn_id.clone(),
            cancel: self.cancel.clone(),
            held: Some(Arc::clone(&self.slot) as Arc<dyn std::any::Any + Send + Sync>),
            // 登记表不知道是哪个用户；提问接口拿到 Permit 之后自己填
            request: None,
            prepared: false,
        }
    }

    /// 这一轮的事件记录。执行过程把事件写进这里，订阅者从这里读。
    pub fn live(&self) -> Arc<LiveTurn> {
        Arc::clone(&self.live)
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Permit({})", self.session_id)
    }
}

impl PartialEq for Permit {
    /// 只比会话 id。有这个是为了让测试能写
    /// `assert_eq!(reg.admit(.., None), Err(..))`——两个 Permit 相不相等本身
    /// 没有业务含义。
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
    }
}
impl Eq for Permit {}

impl Drop for Permit {
    fn drop(&mut self) {
        // ★ 先放会话锁，**再**把事件记录标成结束。
        //
        //   取消接口等的就是「记录结束」，等到了就告诉前端「停好了」，前端
        //   马上在同一个会话里发下一个问题。顺序反过来的话，中间有一小段
        //   记录已经结束、会话锁还没放的空档，那个新问题会撞上 409「上一个
        //   问题还在跑」。
        //
        //   这里也是兜底收尾：执行任务 panic 了的话没人调 finish，订阅者会
        //   永远等下一条事件。
        self.registry.lock().running.remove(&self.session_id);
        self.live.finish();
        // 全局名额在字段 `slot` 被 Drop 时还——要是还有工具线程拿着，
        // 等它们结束才还。
    }
}

/// 一个全局名额。最后一份 `Arc<Slot>` 放手时归还。
struct Slot {
    registry: Arc<Registry>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        inner.slots_in_use = inner.slots_in_use.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拿到名额之后同一个会话再申请会被拒() {
        let reg = Registry::with_capacity(4);
        let _p = reg.admit("s1", None).unwrap();
        match reg.admit("s1", None) {
            Err(Reject::SessionBusy { .. }) => {}
            other => panic!("期望 SessionBusy，实际 {other:?}"),
        }
    }

    #[test]
    fn 名额还回去之后同一个会话能再申请() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", None).unwrap();
        drop(p);
        // ★ 必须绑住这个 Permit。写成 `assert!(reg.admit("s1", None).is_ok())`
        //   的话，Permit 是个临时值，语句结束就被 Drop 掉了，下一句
        //   running_count 读到 0——第一版就是这么假红的。
        //   （顺带说明 guard 真的在工作。）
        let _again = reg.admit("s1", None).unwrap();
        assert_eq!(reg.running_count(), 1);
    }

    #[test]
    fn 上限是几就只能同时跑几个() {
        let reg = Registry::with_capacity(2);
        let _a = reg.admit("s1", None).unwrap();
        let _b = reg.admit("s2", None).unwrap();
        match reg.admit("s3", None) {
            Err(Reject::Capacity { running, .. }) => assert_eq!(running, 2),
            other => panic!("期望 Capacity，实际 {other:?}"),
        }
    }

    /// 默认就允许不同会话同时跑，满了才拒。
    #[test]
    fn 默认允许不同会话同时跑() {
        let reg = Registry::new();
        let permits: Vec<_> = (0..DEFAULT_MAX_TURNS)
            .map(|i| reg.admit(&format!("s{i}"), None).expect("还没满却被拒了"))
            .collect();
        assert_eq!(reg.running_count(), DEFAULT_MAX_TURNS);
        assert!(matches!(
            reg.admit("one-more", None),
            Err(Reject::Capacity { .. })
        ));
        drop(permits);
    }

    /// 同一个人开几个会话都能同时跑——现在没有「每个用户最多几个」。
    /// 登记表只认会话 id，根本不知道谁是谁，这条钉住这件事，
    /// 以后加每用户上限时这条会提醒你改。
    #[test]
    fn 同会话仍然互斥_不同会话不互斥() {
        let reg = Registry::new();
        let _a = reg.admit("s1", None).unwrap();
        let _b = reg.admit("s2", None).expect("不同会话不该互相挡");
        assert!(matches!(
            reg.admit("s1", None),
            Err(Reject::SessionBusy { .. })
        ));
    }

    #[test]
    fn 在跑的会话能拿到事件记录_跑完就没了() {
        let reg = Registry::with_capacity(4);
        assert!(reg.live("s1").is_none());
        let p = reg.admit("s1", None).unwrap();
        let live = reg.live("s1").expect("在跑却拿不到事件记录");
        assert!(Arc::ptr_eq(&live, &p.live()), "拿到的不是同一份记录");
        drop(p);
        assert!(reg.live("s1").is_none());
    }

    /// 名额还回去的时候，事件记录一定被标成结束——不然订阅者会一直等。
    #[test]
    fn 名额归还时事件记录跟着收尾() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", None).unwrap();
        let live = p.live();
        assert!(!live.read_from(0).1);
        drop(p);
        assert!(live.read_from(0).1, "名额还了，订阅者却还会等下去");
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
            let _p = r2.admit("s1", None).unwrap();
            panic!("假装跑挂了");
        }));
        assert!(boom.is_err(), "这个闭包本该 panic");
        assert_eq!(reg.running_count(), 0, "panic 之后名额没还回来");
        assert!(reg.admit("s1", None).is_ok());
    }

    #[test]
    fn 停机之后一律拒绝_但在跑的不受影响() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", None).unwrap();
        reg.stop_accepting();
        assert_eq!(reg.admit("s2", None), Err(Reject::ShuttingDown));
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
        let _p = reg.admit("s1", None).unwrap();
        assert_eq!(reg.wait_until_idle(Duration::from_millis(200)), 1);
    }

    #[test]
    fn 等到空闲_别的线程结束之后就返回() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", None).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(p);
        });
        assert_eq!(reg.wait_until_idle(Duration::from_secs(5)), 0);
    }

    // ── 取消 ───────────────────────────────────────────────

    #[test]
    fn 每一轮申请名额时就分到编号_各不相同() {
        let reg = Registry::with_capacity(4);
        let a = reg.admit("s1", None).unwrap();
        let b = reg.admit("s2", None).unwrap();
        assert!(!a.turn_id().is_empty());
        assert_ne!(a.turn_id(), b.turn_id());
        assert_eq!(a.control().turn_id, a.turn_id());
    }

    /// ★ 编号对不上就什么都不做——迟到的取消请求停不掉后来的新问题。
    #[test]
    fn 编号对得上才会打开取消开关() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", None).unwrap();
        let ctl = p.control();

        assert!(reg.request_cancel("s1", "别的编号").is_none());
        assert!(reg.request_cancel("s2", p.turn_id()).is_none(), "会话不对");
        assert!(!ctl.cancel.is_cancelled(), "编号不对却打开了开关");

        let live = reg.request_cancel("s1", p.turn_id()).expect("编号对得上");
        assert!(ctl.cancel.is_cancelled());
        assert!(Arc::ptr_eq(&live, &p.live()));
    }

    /// 上一轮停完、马上在同一个会话发新问题：新的一轮有新编号，拿旧编号来
    /// 停什么都不会发生。
    #[test]
    fn 拿上一轮的编号停不掉新的一轮() {
        let reg = Registry::with_capacity(4);
        let old = reg.admit("s1", None).unwrap();
        let old_id = old.turn_id().to_string();
        drop(old);
        let new = reg.admit("s1", None).unwrap();
        assert!(reg.request_cancel("s1", &old_id).is_none());
        assert!(!new.control().cancel.is_cancelled());
    }

    /// ★ 会话锁一结束就放；全局名额等丢下的工具线程也结束才还。
    #[test]
    fn 会话锁马上放开_全局名额等工具线程放手才还() {
        let reg = Registry::with_capacity(2);
        let p = reg.admit("s1", None).unwrap();
        // 工具线程手里那一份
        let held = p.control().held.expect("服务模式下要给工具线程一份名额");
        drop(p);

        assert_eq!(reg.running_count(), 0);
        assert_eq!(reg.slots_in_use(), 1, "工具还在跑，名额就还了");
        let again = reg
            .admit("s1", None)
            .expect("这一轮结束了，同一个会话该能马上再问");
        assert_eq!(reg.slots_in_use(), 2);
        assert!(
            matches!(
                reg.admit("s2", None),
                Err(Reject::Capacity { running: 2, .. })
            ),
            "被丢下的工具占着的名额也该算数"
        );

        drop(held);
        assert_eq!(reg.slots_in_use(), 1);
        let _s2 = reg.admit("s2", None).expect("工具线程放手之后名额该回来了");
        drop(again);
    }

    /// ★ 事件记录被标成结束的那一刻，会话锁一定已经放开了。
    ///
    ///   取消接口等的就是「记录结束」，等到就告诉前端停好了，前端马上在同一个
    ///   会话里发下一个问题。顺序反过来的话那个问题会撞 409。
    ///
    ///   空档只有几微秒，靠「多跑几遍碰运气」测不出来（试过，300 遍一次都没
    ///   撞上）。所以换成确定的做法：测试**拿着登记表的锁**，让另一条线程去
    ///   结束这一轮。顺序对的话，它卡在「放会话锁」那一步，记录还没结束；
    ///   顺序反了的话，记录先结束了、它才卡在锁上。
    #[test]
    fn 记录结束时会话锁一定已经放开() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", None).unwrap();
        let live = p.live();

        let guard = reg.lock();
        let ender = std::thread::spawn(move || drop(p));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !live.read_from(0).1,
            "会话锁还没放（登记表被测试拿着），事件记录却已经标成结束了"
        );
        drop(guard);
        ender.join().unwrap();

        assert!(live.read_from(0).1);
        assert!(reg.live("s1").is_none());
    }

    // ── 请求幂等 ───────────────────────────────────────────

    /// ★ 同一次提交的重试撞上了正在跑的自己：告诉它「就是这一轮」，不是「会话忙」。
    ///
    ///   这是那一小段空档的唯一防线：拿到名额之后、写进 turns 表之前，库里
    ///   查不到这一轮，只有登记表知道。
    #[test]
    fn 同一次提交重试撞上自己_认得出是同一轮() {
        let reg = Registry::with_capacity(4);
        let p = reg.admit("s1", Some("req-1")).unwrap();
        assert_eq!(
            reg.admit("s1", Some("req-1")),
            Err(Reject::Duplicate {
                turn_id: p.turn_id().to_string()
            })
        );
        assert_eq!(reg.running_count(), 1, "重试不该再占一个名额");
    }

    #[test]
    fn 不是同一次提交_照旧是会话忙() {
        let reg = Registry::with_capacity(4);
        let _p = reg.admit("s1", Some("req-1")).unwrap();
        assert!(matches!(
            reg.admit("s1", Some("req-2")),
            Err(Reject::SessionBusy { .. })
        ));
        // 命令行没有编号，也不能被当成重复
        assert!(matches!(
            reg.admit("s1", None),
            Err(Reject::SessionBusy { .. })
        ));
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
                match r.admit("same-session", None) {
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

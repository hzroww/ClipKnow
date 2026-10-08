//! 工具在哪儿执行。
//!
//! 循环（`runner.rs`）只管「要调哪个工具、结果怎么记账」，**不管工具在哪条
//! 线程上跑**。这件事交给这里的 [`ToolExecutor`]，有两种：
//!
//! ```text
//! InlineTools    就地执行：在循环自己的线程上跑，跑完才返回。
//!                命令行的 find / ask 和单元测试用。
//! DetachedTools  另开一条线程执行，循环在这边等结果。
//!                网页和 `clipknow turn` 用（都经过 app::turn::execute）。
//! ```
//!
//! ## 为什么要能放到另一条线程上
//!
//! 为了取消。工具一跑可能是几分钟（下载 + 上传 + 视觉分析），用户随时会点
//! 停止。同步代码里没法从外面安全地打断一条线程，所以换个思路：**让工具在
//! 别的线程上跑，循环只是在等它**——要停的时候，循环不等了就行，工具自己
//! 跑到结束，结果作废。这是 Codex 的做法（它每次工具调用也是单独起一个
//! 后台任务，取消时直接丢下）。
//!
//! 这样「能不能被取消」是**循环这一层**的性质，不是每个工具各自的事——
//! 以后加工具，工具代码里一行取消相关的都不用写。
//!
//! ## 为什么不是所有地方都用 DetachedTools
//!
//! 能被丢下的线程，不能借用别人手里的东西（C++ 里 detach 掉的线程捕获了
//! 局部变量的引用，外层函数一返回就是悬垂引用；Rust 编译器直接不让编译）。
//! 所以 DetachedTools 要的是共享所有权的客户端（`Arc`）和一个数据库**路径**
//! （每次工具调用自己开一条连接），而几十个单元测试手里的假客户端都是借来的、
//! 单线程的。命令行的 find / ask 也不需要取消（Ctrl-C 直接结束进程）。
//! 两种执行器跑的是**同一个** [`tools::execute`]，区别只在哪条线程上跑。

use std::any::Any;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use crate::agent::cancel::CancelSignal;
use crate::agent::llm::ToolCall;
use crate::agent::tools::{ToolCtx, ToolOutcome, execute};
use crate::agent::vision::VisionClient;
use crate::ingest::discovery::DiscoveryApi;
use crate::store::sqlite::SqliteStore;

/// 执行一次工具调用。循环里**所有**工具调用都经过这里。
pub trait ToolExecutor {
    /// `vision_budget_left`：这次提问还剩几次视频分析额度，循环维护。
    ///
    /// `cancel`：用户点了停止。能不能在工具跑到一半时不等它，由执行器决定——
    /// 就地执行的做不到（工具就在循环的线程上），另开线程的能做到。
    fn run(&mut self, call: &ToolCall, vision_budget_left: usize, cancel: &CancelSignal)
    -> ToolRun;
}

/// 一次工具调用的结局。
pub enum ToolRun {
    /// 工具跑完了，这是它的结果（成功或失败都在 ToolOutcome 里）。
    Finished(ToolOutcome),
    /// 跑到一半用户停止了，循环不再等它。工具还在后台跑，结果会被扔掉。
    Abandoned,
}

/// 就地执行。借用调用方手里的客户端和数据库连接。
pub struct InlineTools<'a> {
    pub api: &'a dyn DiscoveryApi,
    pub store: &'a mut SqliteStore,
    /// `None` = 没配视觉模型。**不是错误**，那时 fetch_video 降级成只给文字。
    pub vision: Option<&'a dyn VisionClient>,
}

impl ToolExecutor for InlineTools<'_> {
    /// 不看取消信号：工具就在这条线程上，没法一边跑一边不等它。
    /// 跑完之后循环自己会在下一步之前看到信号。
    fn run(&mut self, call: &ToolCall, vision_budget_left: usize, _: &CancelSignal) -> ToolRun {
        ToolRun::Finished(execute(
            &mut ToolCtx {
                api: self.api,
                store: self.store,
                vision: self.vision,
                vision_budget_left,
            },
            call,
        ))
    }
}

/// 等工具结果时，多久看一眼取消信号。
///
/// 用轮询而不是「同时等两件事」：标准库的通道没法和一个开关一起等，而 20 毫秒
/// 看一次的开销可以忽略，用户点停止到循环反应过来最多慢 20 毫秒。
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// 另开一条线程执行。
pub struct DetachedTools {
    api: Arc<dyn DiscoveryApi + Send + Sync>,
    vision: Option<Arc<dyn VisionClient + Send + Sync>>,
    /// 每次工具调用在自己的线程上**新开一条连接**，用这个路径。
    ///
    /// 不和循环共用一条：工具一占几分钟，而循环收尾要写库。SQLite 开连接
    /// 是毫秒级，WAL 模式下多条连接读写互不阻塞（见 `SqliteStore::init`）。
    db_path: String,
    /// 工具线程活着的时候一直拿着它，线程结束才放手。
    ///
    /// 服务模式下这是这一轮的**全局名额**（`app::registry`）。被丢下的工具
    /// 还在后台下载视频、占着内存，名额就不能还——否则用户反复「问、停、
    /// 再问」能叠出任意多个同时进行的下载，「最多同时跑几个」就不再是内存
    /// 的上限了。这一层不知道它是什么，只负责拿着。
    held: Option<Arc<dyn Any + Send + Sync>>,
}

impl DetachedTools {
    pub fn new(
        api: Arc<dyn DiscoveryApi + Send + Sync>,
        vision: Option<Arc<dyn VisionClient + Send + Sync>>,
        db_path: &str,
        held: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Self {
        DetachedTools {
            api,
            vision,
            db_path: db_path.to_string(),
            held,
        }
    }
}

impl ToolExecutor for DetachedTools {
    fn run(
        &mut self,
        call: &ToolCall,
        vision_budget_left: usize,
        cancel: &CancelSignal,
    ) -> ToolRun {
        let (tx, rx) = mpsc::channel();
        let api = Arc::clone(&self.api);
        let vision = self.vision.clone();
        let path = self.db_path.clone();
        let held = self.held.clone();
        let call_for_worker = call.clone();

        let spawned = std::thread::Builder::new()
            .name("clipknow-tool".into())
            .spawn(move || {
                // 线程结束（包括 panic）时才放手，见 `held` 的说明
                let _held = held;
                let call = call_for_worker;
                let out = match SqliteStore::open(&path) {
                    Ok(mut store) => execute(
                        &mut ToolCtx {
                            api: &*api,
                            store: &mut store,
                            vision: vision.as_deref().map(|v| v as &dyn VisionClient),
                            vision_budget_left,
                        },
                        &call,
                    ),
                    Err(e) => ToolOutcome::err(&call.id, format!("工具打开数据库失败: {e}")),
                };
                // 循环已经不等了的话，收的那一头不在了，发不出去就算了——
                // 结果本来就作废。工具写进缓存表（videos / video_dossiers）
                // 的东西照样留着，下次问同一个视频能直接用。
                let _ = tx.send(out);
            });
        if let Err(e) = spawned {
            return ToolRun::Finished(ToolOutcome::err(&call.id, format!("开不了工具线程: {e}")));
        }

        loop {
            match rx.recv_timeout(CANCEL_POLL) {
                Ok(out) => return ToolRun::Finished(out),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        // ★ 不等了。工具线程继续跑到结束（最长就是它自己的
                        //   网络超时：下载、上传 10 分钟，视觉分析 5 分钟），
                        //   结果发到一个没人收的通道里，被扔掉。
                        return ToolRun::Abandoned;
                    }
                }
                // ★ 工具线程没发结果就没了 = 它 panic 了。
                //
                //   在这边**接着 panic**，而不是把它包成一条工具错误：工具挪到
                //   别的线程之前，工具 panic 就是循环 panic，由外层（serve 的
                //   run_one_turn）的 catch_unwind 收尾、把这一轮标成中断。
                //   挪线程不该悄悄改掉这个行为。
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("工具 {} 的执行线程 panic 了", call.name)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::FetchedVideo;
    use crate::error::Result;
    use crate::ingest::discovery::{Endpoint, RawResponse};
    use crate::ingest::url::{ParsedUrl, Platform};
    use crate::store::testdb::TempDb;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    /// 线程安全的假客户端：记下每次调用发生在哪条线程上。
    ///
    /// `gate` 有值时，调用会卡在那里，直到测试往里发一声——用来造一个
    /// 「跑很久的工具」。
    #[derive(Default)]
    struct ThreadSpy {
        threads: Mutex<Vec<std::thread::ThreadId>>,
        panic: bool,
        gate: Option<Mutex<mpsc::Receiver<()>>>,
        /// 卡住的调用最后有没有跑完
        finished: AtomicBool,
    }

    impl ThreadSpy {
        fn blocked() -> (Self, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            (
                ThreadSpy {
                    gate: Some(Mutex::new(rx)),
                    ..Default::default()
                },
                tx,
            )
        }
    }

    impl DiscoveryApi for ThreadSpy {
        fn call(&self, _: Endpoint, p: Platform, arg: &str) -> Result<RawResponse> {
            self.threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            if self.panic {
                panic!("假装工具内部出了 bug");
            }
            if let Some(g) = &self.gate {
                let _ = g.lock().unwrap().recv();
            }
            self.finished.store(true, Ordering::SeqCst);
            Ok(RawResponse {
                endpoint: format!("/fake/{}", p.as_str()),
                body: json!({"videos": [{"id": "v1", "title": format!("搜到的（{arg}）"),
                    "channel": {"title": "某博主", "handle": "somebody", "id": "UC1"}}]}),
            })
        }
        fn fetch_video(&self, _: &ParsedUrl, _: &str) -> Result<FetchedVideo> {
            panic!("这些用例不该走到 fetch_video")
        }
    }

    /// 被 Drop 时把一个开关打开。用来看「名额」什么时候真正被放手。
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn search(q: &str) -> ToolCall {
        ToolCall {
            id: "call_00_x".into(),
            name: "search_videos".into(),
            args: json!({"platform": "youtube", "query": q}),
        }
    }

    fn finished(r: ToolRun) -> ToolOutcome {
        match r {
            ToolRun::Finished(o) => o,
            ToolRun::Abandoned => panic!("没取消却被丢下了"),
        }
    }

    /// 最多等 `limit`，等到 `cond` 成立返回 true。
    fn eventually(limit: Duration, cond: impl Fn() -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn 另开线程执行_结果和就地执行完全一样() {
        let db = TempDb::new("exec-same");
        let call = search("科普");
        let never = CancelSignal::new();

        let api = Arc::new(ThreadSpy::default());
        let a = finished(DetachedTools::new(api, None, db.path(), None).run(&call, 0, &never));

        let mut store = SqliteStore::open(db.path()).unwrap();
        let spy = ThreadSpy::default();
        let b = finished(
            InlineTools {
                api: &spy,
                store: &mut store,
                vision: None,
            }
            .run(&call, 0, &never),
        );

        assert_eq!(a.result.content, b.result.content);
        assert_eq!(a.result.is_error, b.result.is_error);
        assert_eq!(a.external_calls, b.external_calls);
        assert_eq!(a.endpoint, b.endpoint);
    }

    #[test]
    fn 工具真的跑在另一条线程上() {
        let db = TempDb::new("exec-thread");
        let api = Arc::new(ThreadSpy::default());
        DetachedTools::new(api.clone(), None, db.path(), None).run(
            &search("x"),
            0,
            &CancelSignal::new(),
        );

        let seen = api.threads.lock().unwrap();
        assert_eq!(seen.len(), 1, "工具应该被调了一次");
        assert_ne!(
            seen[0],
            std::thread::current().id(),
            "工具还在循环的线程上跑——那样循环就没法不等它"
        );
    }

    /// 挪线程之前工具 panic 就是循环 panic，挪了之后也得是。
    #[test]
    fn 工具线程_panic_了_调用方也跟着_panic() {
        let db = TempDb::new("exec-panic");
        let api = Arc::new(ThreadSpy {
            panic: true,
            ..Default::default()
        });
        let mut ex = DetachedTools::new(api, None, db.path(), None);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ex.run(&search("x"), 0, &CancelSignal::new())
        }));
        assert!(r.is_err(), "工具 panic 被悄悄吞成了一条普通结果");
    }

    #[test]
    fn 数据库打不开时给一条配对的错误结果_不中止() {
        let api = Arc::new(ThreadSpy::default());
        let out = finished(
            DetachedTools::new(api, None, "/nonexistent-dir/x.db", None).run(
                &search("x"),
                0,
                &CancelSignal::new(),
            ),
        );
        assert!(out.result.is_error);
        assert_eq!(
            out.result.call_id, "call_00_x",
            "错误结果也要和这次调用配对"
        );
    }

    /// 没人取消时，工具跑多久就等多久——不能因为轮询超时就当成取消。
    #[test]
    fn 没取消时跑得比轮询间隔久也照常等到结果() {
        let db = TempDb::new("exec-slow");
        let (spy, release) = ThreadSpy::blocked();
        let api = Arc::new(spy);
        std::thread::spawn(move || {
            std::thread::sleep(CANCEL_POLL * 10);
            let _ = release.send(());
        });
        let out = finished(DetachedTools::new(api, None, db.path(), None).run(
            &search("x"),
            0,
            &CancelSignal::new(),
        ));
        assert!(!out.result.is_error);
    }

    /// ★ 「以后加什么工具都卡不住取消」的证明：一个**不放行就永远不返回**的
    ///   工具，点停止后循环必须很快不再等它。
    ///
    ///   顺带钉住名额：被丢下的工具还在后台跑，它拿着的东西（服务模式下是
    ///   全局名额）要等它**真的跑完**才放手。
    #[test]
    fn 工具卡住时取消_很快不再等它_名额等工具真跑完才放手() {
        let db = TempDb::new("exec-abandon");
        let (spy, release) = ThreadSpy::blocked();
        let api = Arc::new(spy);
        let released = Arc::new(AtomicBool::new(false));
        let held: Arc<dyn Any + Send + Sync> = Arc::new(DropFlag(Arc::clone(&released)));
        let mut ex = DetachedTools::new(api.clone(), None, db.path(), Some(held));

        let cancel = CancelSignal::new();
        let c2 = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.cancel();
            Instant::now()
        });

        // ★ run 放到另一条线程上、带超时地等它。执行器要是**没在看**取消信号，
        //   run 就永远不返回——直接在测试线程上调的话，这条测试会卡死而不是
        //   变红（CI 上那是六小时超时）。故障注入时真卡过一次。
        let (done_tx, done_rx) = mpsc::channel();
        let runner = std::thread::spawn(move || {
            let r = ex.run(&search("x"), 0, &cancel);
            let _ = done_tx.send((matches!(r, ToolRun::Abandoned), Instant::now()));
            ex
        });
        let Ok((abandoned, returned_at)) = done_rx.recv_timeout(Duration::from_secs(5)) else {
            // 放行卡住的工具，让两条线程都能收摊，再判红
            let _ = release.send(());
            panic!("取消 5 秒后还在等工具——执行器没在看取消信号");
        };
        let cancelled_at = canceller.join().unwrap();
        let ex = runner.join().unwrap();

        assert!(abandoned, "取消之后还在等工具");
        assert!(
            returned_at.duration_since(cancelled_at) < Duration::from_millis(500),
            "取消后 {:?} 才不等，太慢",
            returned_at.duration_since(cancelled_at)
        );

        // 执行器自己的那份也扔掉，只剩工具线程手里那份
        drop(ex);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !released.load(Ordering::SeqCst),
            "工具还在后台跑，名额却已经放手了"
        );
        assert!(!api.finished.load(Ordering::SeqCst));

        // 工具跑完 → 名额放手
        release.send(()).unwrap();
        assert!(
            eventually(Duration::from_secs(5), || released.load(Ordering::SeqCst)),
            "工具跑完了，名额还没放手"
        );
        assert!(
            api.finished.load(Ordering::SeqCst),
            "被丢下的工具应该自己跑完"
        );
    }

    /// 就地执行做不到半路不等。信号打开了也照常跑完，交给循环在下一步之前处理。
    #[test]
    fn 就地执行不看取消信号() {
        let mut store = SqliteStore::in_memory().unwrap();
        let spy = ThreadSpy::default();
        let cancel = CancelSignal::new();
        cancel.cancel();
        let r = InlineTools {
            api: &spy,
            store: &mut store,
            vision: None,
        }
        .run(&search("x"), 0, &cancel);
        assert!(matches!(r, ToolRun::Finished(_)));
    }
}

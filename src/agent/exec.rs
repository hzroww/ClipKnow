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

use std::sync::Arc;
use std::sync::mpsc;

use crate::agent::llm::ToolCall;
use crate::agent::tools::{ToolCtx, ToolOutcome, execute};
use crate::agent::vision::VisionClient;
use crate::ingest::discovery::DiscoveryApi;
use crate::store::sqlite::SqliteStore;

/// 执行一次工具调用。循环里**所有**工具调用都经过这里。
pub trait ToolExecutor {
    /// `vision_budget_left`：这次提问还剩几次视频分析额度，循环维护。
    fn run(&mut self, call: &ToolCall, vision_budget_left: usize) -> ToolOutcome;
}

/// 就地执行。借用调用方手里的客户端和数据库连接。
pub struct InlineTools<'a> {
    pub api: &'a dyn DiscoveryApi,
    pub store: &'a mut SqliteStore,
    /// `None` = 没配视觉模型。**不是错误**，那时 fetch_video 降级成只给文字。
    pub vision: Option<&'a dyn VisionClient>,
}

impl ToolExecutor for InlineTools<'_> {
    fn run(&mut self, call: &ToolCall, vision_budget_left: usize) -> ToolOutcome {
        execute(
            &mut ToolCtx {
                api: self.api,
                store: self.store,
                vision: self.vision,
                vision_budget_left,
            },
            call,
        )
    }
}

/// 另开一条线程执行。
pub struct DetachedTools {
    api: Arc<dyn DiscoveryApi + Send + Sync>,
    vision: Option<Arc<dyn VisionClient + Send + Sync>>,
    /// 每次工具调用在自己的线程上**新开一条连接**，用这个路径。
    ///
    /// 不和循环共用一条：工具一占几分钟，而循环收尾要写库。SQLite 开连接
    /// 是毫秒级，WAL 模式下多条连接读写互不阻塞（见 `SqliteStore::init`）。
    db_path: String,
}

impl DetachedTools {
    pub fn new(
        api: Arc<dyn DiscoveryApi + Send + Sync>,
        vision: Option<Arc<dyn VisionClient + Send + Sync>>,
        db_path: &str,
    ) -> Self {
        DetachedTools {
            api,
            vision,
            db_path: db_path.to_string(),
        }
    }
}

impl ToolExecutor for DetachedTools {
    fn run(&mut self, call: &ToolCall, vision_budget_left: usize) -> ToolOutcome {
        let (tx, rx) = mpsc::channel();
        let api = Arc::clone(&self.api);
        let vision = self.vision.clone();
        let path = self.db_path.clone();
        let call_for_worker = call.clone();

        let spawned = std::thread::Builder::new()
            .name("clipknow-tool".into())
            .spawn(move || {
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
                // 收的那一头可能已经不在了（以后循环不等它的时候），发不出去
                // 就算了——结果本来就作废。
                let _ = tx.send(out);
            });
        if let Err(e) = spawned {
            return ToolOutcome::err(&call.id, format!("开不了工具线程: {e}"));
        }

        match rx.recv() {
            Ok(out) => out,
            // ★ 工具线程没发结果就没了 = 它 panic 了。
            //
            //   在这边**接着 panic**，而不是把它包成一条工具错误：工具挪到别的
            //   线程之前，工具 panic 就是循环 panic，由外层（serve 的
            //   run_one_turn）的 catch_unwind 收尾、把这一轮标成中断。挪线程
            //   不该悄悄改掉这个行为。
            Err(mpsc::RecvError) => panic!("工具 {} 的执行线程 panic 了", call.name),
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

    /// 线程安全的假客户端：记下每次调用发生在哪条线程上。
    #[derive(Default)]
    struct ThreadSpy {
        threads: Mutex<Vec<std::thread::ThreadId>>,
        panic: bool,
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

    fn search(q: &str) -> ToolCall {
        ToolCall {
            id: "call_00_x".into(),
            name: "search_videos".into(),
            args: json!({"platform": "youtube", "query": q}),
        }
    }

    #[test]
    fn 另开线程执行_结果和就地执行完全一样() {
        let db = TempDb::new("exec-same");
        let call = search("科普");

        let api = Arc::new(ThreadSpy::default());
        let mut detached = DetachedTools::new(api.clone(), None, db.path());
        let a = detached.run(&call, 0);

        let mut store = SqliteStore::open(db.path()).unwrap();
        let spy = ThreadSpy::default();
        let b = InlineTools {
            api: &spy,
            store: &mut store,
            vision: None,
        }
        .run(&call, 0);

        assert_eq!(a.result.content, b.result.content);
        assert_eq!(a.result.is_error, b.result.is_error);
        assert_eq!(a.external_calls, b.external_calls);
        assert_eq!(a.endpoint, b.endpoint);
    }

    #[test]
    fn 工具真的跑在另一条线程上() {
        let db = TempDb::new("exec-thread");
        let api = Arc::new(ThreadSpy::default());
        DetachedTools::new(api.clone(), None, db.path()).run(&search("x"), 0);

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
        let mut ex = DetachedTools::new(api, None, db.path());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex.run(&search("x"), 0)));
        assert!(r.is_err(), "工具 panic 被悄悄吞成了一条普通结果");
    }

    #[test]
    fn 数据库打不开时给一条配对的错误结果_不中止() {
        let api = Arc::new(ThreadSpy::default());
        let out = DetachedTools::new(api, None, "/nonexistent-dir/x.db").run(&search("x"), 0);
        assert!(out.result.is_error);
        assert_eq!(
            out.result.call_id, "call_00_x",
            "错误结果也要和这次调用配对"
        );
    }
}

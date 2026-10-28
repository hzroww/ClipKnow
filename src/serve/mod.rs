//! 常驻 HTTP 服务。**给 Go 那边的 web 服务调用的，不对外网开放。**
//!
//! ## 为什么要这个东西
//!
//! 在这之前，Go 每收到一次提问就 `exec.Command` 起一个 `clipknow turn`
//! 子进程，答完进程就死。三件事因此做不了：
//!
//!   1. **并发。** 「现在有哪几个执行在跑、属于哪个会话、取消信号发给谁」
//!      这些状态必须和执行待在一起，而进程之间什么都不共享。
//!   2. **一份真相。** `sessions` / `turns` / `items` 三张表 Rust 写、Go 读，
//!      两边都得懂表结构，改一列要改两个地方。
//!   3. **问状态。** 进程死了就问不到「跑到哪一步了」「能不能取消」。
//!
//! 常驻之后，会话的增删查改和执行管理全在这一个进程里，Go 只管账号和登录。
//!
//! ## 线程模型（重要）
//!
//! ```text
//! tokio 多线程 runtime
//!  ├─ worker 线程（= CPU 核数）   只收发 HTTP、推 SSE，永远不阻塞
//!  └─ blocking 线程池（上限 8）   真正干活的地方
//! ```
//!
//! 这个项目的核心是**同步**代码：`reqwest::blocking` 打模型和 ScrapeCreators，
//! `rusqlite` 读写 SQLite。同步代码在 async 函数里直接调用会把那条 worker
//! 线程占住——worker 只有几条，占满之后整个服务连 `/internal/health` 都答不了，
//! 而且**不会报任何错**，表现就是「卡住」。
//!
//! 所以规矩是：**凡是碰库、碰网络的活，一律 `spawn_blocking`。**
//! 包括下面那两个亚毫秒级的会话查询——它们看着快，但 SQLite 撞锁时会等到
//! `busy_timeout`（5 秒），那就不是「快」了。
//!
//! blocking 池上限**显式设成 8**，而不是用 tokio 默认的 512。512 的意思是
//! 「积压 512 个请求也照单全收」，那是把拒绝的时机从「立刻」推迟到「内存耗尽」。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::json;
use tokio_stream::StreamExt;

use crate::app::registry::Reject;
use crate::error::{ClipKnowError, Result};
use crate::store::sqlite::{ChatRequestClaim, SqliteStore};
use crate::wire::TurnSink;

mod admin;
pub mod auth;
use auth::Principal;

/// 默认监听地址。
///
/// **绑 127.0.0.1 而不是 0.0.0.0**：这个服务信任的是「凭证签对了」，
/// 而凭证的密钥是共享的，泄漏一次全完。多一道「外网根本连不上」的物理
/// 隔离，代价是零。
///
/// 端口选 3100 而不是 3001：3000 是 Go 那边的 web 服务，3001 太容易和
/// 别的开发服务撞（Next.js 一被占就自动挪到 3001）。
pub const DEFAULT_ADDR: &str = "127.0.0.1:3100";

/// blocking 线程池上限。见模块文档最后一段。
const MAX_BLOCKING_THREADS: usize = 8;

/// `--max-turns` 最多能设多少。
///
/// ★ 必须比 blocking 线程池**小**，而且要留出余量。
///
///   每个在跑的提问会占住一条 blocking 线程**整整几分钟**。而会话列表、
///   聊天记录、归属校验、健康检查这些查询也走 blocking 线程。提问把池子
///   占满的话，这些查询就得排在「某个提问答完」之后——表现是网页的会话
///   列表转圈几分钟，健康检查超时，Docker 把容器判成 unhealthy。
///
///   留 2 条给查询：它们都是亚毫秒级的，2 条绰绰有余。
pub const MAX_TURNS_LIMIT: usize = MAX_BLOCKING_THREADS - 2;

/// 服务的共享状态。所有 handler 通过 `State<Arc<AppState>>` 拿到它。
pub struct AppState {
    /// **只读查询共用的那一条连接。**
    ///
    /// `rusqlite::Connection` 不是 `Sync`，多个线程不能同时用同一条。
    /// 三个选择：连接池、`Mutex` 串起来、每次现开一条。选了 `Mutex`：
    ///
    ///   - 一次会话列表查询是亚毫秒级，串起来的排队代价可以忽略；
    ///   - 连接池要多一个依赖（r2d2_sqlite），而它跟 rusqlite 的版本
    ///     经常对不上（这里用的 0.40 很新）；
    ///   - 每次现开一条要重跑 `PRAGMA foreign_keys` / `journal_mode` /
    ///     `busy_timeout`，开销比查询本身还大。
    ///
    /// ⚠️ **正在跑的那次提问不用这条连接。** 它要 `&mut SqliteStore` 并且
    /// 一占几分钟，占的是这条的话，会话列表就打不开了。执行任务在自己的
    /// blocking 线程里另开一条——WAL 模式下读不挡写、写不挡读。
    read: Mutex<SqliteStore>,

    /// 数据库文件路径。执行任务要拿它自己开连接。
    pub db_path: String,

    /// 内部凭证的校验器。见 [`auth`] 模块。
    pub verifier: auth::Verifier,

    /// 执行准入登记表：现在有哪几次提问在跑。
    /// 策略：同一个会话一次一个，全局最多 `--max-turns` 个（默认 4）。
    pub registry: Arc<crate::app::registry::Registry>,

    /// 取消接口最多等多久。见 [`CANCEL_WAIT`]。
    cancel_wait: std::time::Duration,
}

/// 取消接口最多等这一轮停下多久。
///
/// 大部分情况几十毫秒就停了：在两步之间、或者在跑工具（循环直接不等工具）。
/// 等得久只有一种情况——模型正在说话，规则是让它说完（`agent::cancel`），
/// 那可能要十几二十秒。等满了返回 202「正在收尾」，**不是错误**：开关已经
/// 打开了，它说完就会停，前端接着看事件流里的结束事件就行。
pub const CANCEL_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

impl AppState {
    pub fn new(store: SqliteStore, db_path: String, verifier: auth::Verifier) -> Self {
        AppState {
            verifier,
            registry: crate::app::registry::Registry::new(),
            read: Mutex::new(store),
            db_path,
            cancel_wait: CANCEL_WAIT,
        }
    }

    /// 改取消接口的等待上限。测试用——不然测一次「等满了」要 15 秒。
    pub fn with_cancel_wait(mut self, d: std::time::Duration) -> Self {
        self.cancel_wait = d;
        self
    }

    /// 改全局并发上限。`clipknow serve --max-turns` 用它。
    pub fn with_max_turns(mut self, n: usize) -> Self {
        self.registry = crate::app::registry::Registry::with_capacity(n);
        self
    }

    /// 借用只读连接。
    ///
    /// **中毒了也照常继续**（`into_inner`）。`Mutex` 中毒的意思是「上一个
    /// 持有者 panic 了」；对一条只读的 SQLite 连接来说，那不会留下半截事务
    /// 或者坏掉的状态。而默认行为（`unwrap` 直接 panic）会让**一次**读查询
    /// 的 panic 永久废掉整个服务的所有读——那个后果比原来的 bug 严重得多。
    ///
    /// ★ 调用方必须在 `spawn_blocking` 里用它，不能在 async 函数里直接拿。
    pub fn read(&self) -> MutexGuard<'_, SqliteStore> {
        self.read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ── 统一的错误响应 ──────────────────────────────────────────
//
// 所有 /internal/* 出错时都长这样：
//
//   {"code": "session_busy", "message": "上一个问题还在跑（已经 23 秒）"}
//
// `code` 是给程序 match 的稳定标识，`message` 是给人看的。两个都给，是因为
// Go 那边要按 code 决定 HTTP 状态，而浏览器上要显示 message——只给一个的话，
// 另一边就得自己编，那就成了两份真相。

/// 一个错误响应。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    /// 服务端自己的问题（开库失败、写库失败）。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// 路由没匹配上。
    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "没有这个接口")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"code": self.code, "message": self.message});
        (self.status, axum::Json(body)).into_response()
    }
}

/// `spawn_blocking` 的 join 失败 = 那个闭包 panic 了。
///
/// 单独一个构造函数是为了让这句话只写一遍——每个走 blocking 的 handler
/// 都要处理它，而 `JoinError` 的默认 Display（"task panicked"）在浏览器上
/// 是句废话。
fn join_failed(e: tokio::task::JoinError) -> ApiError {
    ApiError::internal(format!("内部任务异常退出: {e}"))
}

impl From<ClipKnowError> for ApiError {
    fn from(e: ClipKnowError) -> Self {
        match e {
            ClipKnowError::FreeQuotaExhausted => ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "free_quota_exhausted",
                e.to_string(),
            ),
            // 调用方传的参数不对，不是服务端的问题
            ClipKnowError::BadRequest(m) => {
                ApiError::new(StatusCode::BAD_REQUEST, "bad_request", m)
            }
            other => ApiError::internal(other.to_string()),
        }
    }
}

// ── 路由 ────────────────────────────────────────────────────

/// 路由表。
///
/// 抽成函数（而不是在 `run` 里内联）是为了让测试能起一个**一模一样**的服务。
/// 测试里自己再列一遍路由的话，这里加了接口那边忘了加，测试就会悄悄漏掉它。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // 不设防：Docker 的 healthcheck 调它，那时候没有密钥可用。
        // 它也确实什么都不泄漏——只说库的迁移版本。
        .route("/internal/health", get(health))
        .route("/internal/quota", get(quota))
        // 往下都要内部凭证。靠 handler 参数里的 `Principal` 提取器强制，
        // 不是靠中间件——见 auth.rs 里关于「忘了调」的说明。
        .route("/internal/whoami", get(whoami))
        .route(
            "/internal/admin/users/{user}/sessions",
            get(admin::sessions),
        )
        .route(
            "/internal/admin/sessions/{session}/turns",
            get(admin::turns),
        )
        .route("/internal/admin/turns/{turn}", get(admin::turn))
        .route(
            "/internal/admin/turns/{turn}/items/{item}/raw",
            get(admin::raw),
        )
        .route("/internal/sessions", get(list_sessions))
        // 提问（新会话的话顺便建会话）。为什么合成一个，见 start_turn
        .route("/internal/turns", post(start_turn))
        // axum 0.8 的路径参数是 {id}，不是老版本的 :id
        .route("/internal/sessions/{id}/messages", get(session_messages))
        .route("/internal/sessions/{id}/events", get(session_events))
        .route(
            "/internal/sessions/{id}/turns/{turn}/cancel",
            post(cancel_turn),
        )
        // 长期记忆：网页上的记忆面板列出、删除。只有这个用户自己的
        .route("/internal/memories", get(list_memories))
        .route("/internal/memories/{id}", delete(delete_memory))
        .fallback(fallback)
        .with_state(state)
}

async fn fallback() -> ApiError {
    ApiError::not_found()
}

/// `GET /internal/health`
///
/// 返回 `{"ok":true,"schema_version":3}`。
///
/// 三个用处：
///   1. Go 启动时探一次——配错地址在第一秒就说清楚，不用等第一次提问；
///   2. Docker 的 healthcheck，让 web 容器能 `depends_on` 它；
///   3. 测试里等服务起来。
///
/// ★ 它**真的查一次库**（读 `schema_migrations` 的当前版本），不是返回一个
///   写死的常量。「进程活着」和「进程能用」是两回事：库文件被删了、权限
///   不对、被别的写者锁死超过 busy_timeout，进程都还活得好好的。
async fn health(State(st): State<Arc<AppState>>) -> std::result::Result<Response, ApiError> {
    let (running, slots, max_turns, accepting) = (
        st.registry.running_count(),
        st.registry.slots_in_use(),
        st.registry.max_total(),
        st.registry.is_accepting(),
    );
    let version = tokio::task::spawn_blocking(move || st.read().schema_version())
        .await
        .map_err(join_failed)??;
    Ok(axum::Json(json!({
        "ok": true,
        "schema_version": version,
        "running_turns": running,
        // 全局名额用掉了几个。可能比 running_turns 多：被停止的提问丢下的
        // 工具还在后台跑时，名额还没还（见 app::registry）。
        "slots_in_use": slots,
        "max_turns": max_turns,
        // 停机过程中变 false。healthcheck 不看它——那时候该让在跑的
        // 跑完，而不是让编排系统立刻重启容器。
        "accepting": accepting,
    }))
    .into_response())
}

/// `GET /internal/whoami`
///
/// 返回 `{"user_id":"..."}` —— 就是凭证里的 `sub`。不查库。
///
/// 存在的理由是**让「密钥配错了」在启动时就暴露**。`/internal/health` 不设防，
/// 所以它通了只说明「端口通、库能读」；密钥两边不一致要等到第一次提问才
/// 报 401，那时候用户已经在等答案了。Go 启动时探一次这个接口，配错就直接
/// 起不来（web/main.go）。
///
/// 顺带也是 `Principal` 提取器的唯一真实消费者，让它被真的 HTTP 请求走一遍，
/// 而不只是单元测试里直接调 `verify()`。
async fn whoami(who: Principal) -> Response {
    axum::Json(json!({"user_id": who.user_id})).into_response()
}

/// `GET /internal/sessions?limit=200`
///
/// 返回 `{"sessions":[{id,title,created_at}]}` —— **只有这个用户的**。
/// 过滤写在 SQL 的 WHERE 里（`SqliteStore::list_sessions_for_user`），
/// 不是查出来再筛。
///
/// 外面套一层对象而不是直接返回数组：顶层是数组的话，以后想加
/// `next_cursor` 分页游标就是破坏性变更。给浏览器的那一层形状由 Go 决定，
/// 它现在仍然返回裸数组（前端没动）。
async fn list_sessions(
    State(st): State<Arc<AppState>>,
    who: Principal,
    Query(q): Query<ListQuery>,
) -> std::result::Result<Response, ApiError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_SESSION_LIMIT)
        .clamp(1, MAX_SESSION_LIMIT);
    let items = tokio::task::spawn_blocking(move || {
        crate::app::sessions::list(&st.read(), &who.user_id, limit)
    })
    .await
    .map_err(join_failed)??;
    Ok(axum::Json(json!({"sessions": items})).into_response())
}

/// `GET /internal/sessions/{id}/messages`
///
/// 返回 `{"messages":[{role,text,seq,failed}]}`。
///
/// 不是自己的会话、不存在的会话、已软删的会话，**一律 404**，
/// 不区分——区分开就等于告诉对方「有这么个会话，只是不给你看」。
async fn session_messages(
    State(st): State<Arc<AppState>>,
    who: Principal,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let found = tokio::task::spawn_blocking(move || {
        crate::app::sessions::messages(&st.read(), &who.user_id, &session_id)
    })
    .await
    .map_err(join_failed)??;
    match found {
        Ok(msgs) => Ok(axum::Json(json!({"messages": msgs})).into_response()),
        Err(crate::app::sessions::NotFound) => Err(not_found_session()),
    }
}

/// `POST /internal/turns`
///
/// 提问。请求体：
///
/// ```json
/// {"request_id":"浏览器点发送时生成的编号", "session_id":"在哪个会话里问（空=新会话）",
///  "question":"...", "provider":"deepseek"}
/// ```
///
/// 返回三种：
///
/// ```text
/// 新的一轮开始跑     200 text/event-stream，和改之前一模一样
/// 同一次提交的重试   200 application/json {"duplicate":true,"session":…,"turn":…,"status":…}
///                    不重跑、不重开事件流；还在跑的话调用方去「接着看」
/// 拒绝               400 参数错 / 404 不是你的会话 / 409 会话忙或编号冲突 / 503 名额满
/// ```
///
/// ## 为什么建会话和提问合成一个接口
///
/// 以前 Go 分两次调：先建会话，再提问。新会话里提问、第二次调用断了，浏览器
/// 带着同一个编号重试——Go 什么都不记得，又去建一个会话，同一个问题就挂在两个
/// 会话上了。要让「建会话」认得出是同一次提交，就得在建会话的那一刻把提交
/// 记录（chat_requests）写下去，而那一行要存问题原文，建会话那次调用手里没有。
/// 合成一次调用之后，建会话和写记录在同一个事务里（[`SqliteStore::claim_chat_request`]）。
///
/// **什么时候建会话的规则没变**：带了会话就在那个会话里问，没带才建。
///
/// ## 流开始之前的拒绝走 HTTP 状态码
///
/// 设计文档第 6.2 节：「HTTP 头发出前的拒绝使用 HTTP 状态码；开始流之后
/// 用事件报告错误」。混着来的话，Go 和前端都得同时处理两套错误路径。
///
/// ## 浏览器断了怎么办
///
/// **什么都不做，执行照常跑完并落库。** 那一轮已经花了 SC 配额、模型 token、
/// 可能还有一次视频分析的钱，为了「你关了页面」把这些扔掉是最亏的。
async fn start_turn(
    State(st): State<Arc<AppState>>,
    who: Principal,
    axum::Json(body): axum::Json<TurnBody>,
) -> std::result::Result<Response, ApiError> {
    let question = body.question.trim().to_string();
    if question.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "问题是空的",
        ));
    }
    let request_id = body.request_id.trim().to_string();
    if !valid_request_id(&request_id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "request_id 必须是 1–64 位字母、数字、短横线或下划线",
        ));
    }
    // provider 来自浏览器，是外部输入。parse_provider 只认白名单里那两个，
    // 拼错直接报错而不是悄悄用默认那家。
    let provider = crate::app::turn::parse_provider(body.provider.as_deref())?;
    // 空串当作「没给」，和 provider 一样（见 parse_provider 里的说明）
    let wanted_session = body
        .session_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // ★ 查（必要时建）这次提交的记录。归属校验在里面，而且排在申请名额**之前**：
    //   反过来的话，拿别人的会话 id 乱发请求就能把名额占掉——一个不用登录成
    //   别人就能做到的拒绝服务。
    let claim = {
        let st = Arc::clone(&st);
        let (uid, rid, q) = (who.user_id.clone(), request_id.clone(), question.clone());
        tokio::task::spawn_blocking(move || {
            st.read()
                .claim_chat_request(&uid, &rid, wanted_session.as_deref(), &q)
        })
        .await
        .map_err(join_failed)??
    };
    let session_id = match claim {
        ChatRequestClaim::Proceed { session_id } => session_id,
        ChatRequestClaim::Duplicate {
            session_id,
            turn_id,
        } => return duplicate_response(&st, session_id, turn_id).await,
        ChatRequestClaim::NotFound => return Err(not_found_session()),
        ChatRequestClaim::Conflict => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "request_conflict",
                "这个提交编号已经用在另一个问题上了",
            ));
        }
    };

    // 申请名额。拿不到就**立刻拒绝，不排队**——排队的话用户点了发送、
    // 界面一动不动，分不清是卡了还是坏了。
    //
    // 被拒的话 chat_requests 里那一行留着、turn_id 是空的：重试时接着用同一个
    // 会话，不会再建一个。
    let permit = match st.registry.admit(&session_id, Some(&request_id)) {
        Ok(p) => p,
        Err(Reject::Duplicate { turn_id }) => {
            return duplicate_response(&st, session_id, turn_id).await;
        }
        Err(r) => {
            let status = match r {
                Reject::SessionBusy { .. } => StatusCode::CONFLICT,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            return Err(ApiError::new(status, r.code(), r.message()));
        }
    };

    // 这一轮的事件记录。执行过程往里写，订阅者（发起提问的这个连接、
    // 以及刷新后重新连上来的页面）从里面读。
    let live = permit.live();
    // 编号、取消开关、全局名额。编号在这一刻就定了，比写库早。
    let mut control = permit.control();
    control.request = Some(crate::app::turn::ChatRequestRef {
        user_id: who.user_id.clone(),
        request_id,
    });
    let db_path = st.db_path.clone();
    let sink = Arc::clone(&live);

    // ★ 真正干活的在 blocking 线程上。
    //
    //   一次提问是几十秒到几分钟的**同步**代码（reqwest::blocking 打模型
    //   和 ScrapeCreators，rusqlite 写库）。放在 async worker 线程上会把
    //   整个 HTTP 服务占死，而且不报任何错——表现就是"卡住"。
    //
    //   名额（permit）move 进闭包：闭包结束时 Drop，名额归还。panic 也走
    //   Drop，所以"每条退出路径都释放"是类型系统保证的。
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // Keep admission and execution in one owned task. A disconnected HTTP
        // client must not leave a charged question without an execution worker.
        let prepare = (|| -> crate::error::Result<()> {
            let llm = crate::agent::llm::build_client(provider)?;
            crate::ingest::scrapecreators::ScrapeCreators::from_env()?;
            let mut store = SqliteStore::open(&db_path)?;
            let request = control.request.as_ref().expect("HTTP request identity");
            store.begin_turn(
                &control.turn_id,
                &session_id,
                llm.model_name(),
                &question,
                &question.chars().take(40).collect::<String>(),
                Some((&request.user_id, &request.request_id)),
            )?;
            Ok(())
        })();
        match prepare {
            Ok(()) => {
                control.prepared = true;
                let _ = ready_tx.send(Ok(()));
            }
            Err(e) => {
                let _ = ready_tx.send(Err(ApiError::from(e)));
                return;
            }
        }
        run_one_turn(&db_path, &session_id, &question, provider, &control, &sink);
        // _permit 在这里 Drop：先放会话锁、再把事件记录标成结束（顺序见
        // Permit 的 Drop）。已经连上的订阅者手里有自己的 Arc，会把剩下的读完。
        // 全局名额要等这一轮丢下的工具线程（如果有）也结束才还。
    });
    ready_rx
        .await
        .map_err(|_| ApiError::internal("问答准备任务异常退出"))??;
    Ok(subscribe_sse(live))
}

async fn quota(
    State(st): State<Arc<AppState>>,
    who: Principal,
) -> std::result::Result<axum::Json<serde_json::Value>, ApiError> {
    let value = tokio::task::spawn_blocking(move || st.read().user_quota(&who.user_id))
        .await
        .map_err(join_failed)??;
    Ok(axum::Json(value))
}

/// 同一次提交的重试：告诉调用方「就是这一轮」，带上它现在的状态。
///
/// 返回 JSON 而不是重新接上事件流：调用方（前端）看到 running 就自己去
/// 「接着看」（`GET /internal/sessions/{id}/events`），看到终态就去拉聊天记录。
/// 两条路都是现成的，这里不再做一套。
async fn duplicate_response(
    st: &Arc<AppState>,
    session_id: String,
    turn_id: String,
) -> std::result::Result<Response, ApiError> {
    let status = {
        let st = Arc::clone(st);
        let (sid, tid) = (session_id.clone(), turn_id.clone());
        tokio::task::spawn_blocking(move || st.read().turn_status(&sid, &tid))
            .await
            .map_err(join_failed)??
    };
    // 登记表认出来的那种（拿到名额、还没写库）库里还没有这一行：就是在跑
    let status = status.map_or("running", |s| s.as_str());
    Ok(axum::Json(json!({
        "duplicate": true,
        "session": session_id,
        "turn": turn_id,
        "status": status,
    }))
    .into_response())
}

/// 提交编号的格式。浏览器生成的是 UUID，这里放宽到常见的安全字符，
/// 但限长：它要进库、进主键。
fn valid_request_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 在 blocking 线程上跑一轮，并且**无论怎么结束都把库收拾干净**。
///
/// 「收拾干净」指的是：提问一被接受，库里就有一行 status='running'
/// （见 `SqliteStore::begin_turn`）。正常结束时 finish_turn 会把它改成终态；
/// 但执行 panic 了、或者收尾落库本身失败了，它就会一直停在 running，
/// 界面上永远显示「正在回答」。所以最后一定补一句：这个会话里要是还有
/// running 的，改成 interrupted。
///
/// 这一步必须在名额归还**之前**做（调用方保证）——名额还没还，同一个会话
/// 就不可能有新的一轮开始，不会误伤别人。
fn run_one_turn(
    db_path: &str,
    sid: &str,
    question: &str,
    provider: Option<crate::agent::llm::Provider>,
    control: &crate::app::turn::TurnControl,
    sink: &crate::app::live::LiveTurn,
) {
    // ★ 这个执行**自己开一条连接**，不用 AppState 里那条只读的。
    //   它要 &mut SqliteStore 并且一占几分钟，占那条的话会话列表就
    //   打不开了。WAL 模式下读不挡写、写不挡读。
    let mut store = match SqliteStore::open(db_path) {
        Ok(s) => s,
        Err(e) => {
            sink.emit_json(&crate::wire::error_json(&format!("开库失败: {e}")));
            return;
        }
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cfg = crate::agent::runner::LoopConfig::default();
        let req = crate::app::turn::TurnRequest {
            session_id: sid,
            db_path,
            question,
            provider,
            config: &cfg,
            control,
            // 用户已经拿到答案了，抽取不让他等
            extraction: crate::app::turn::Extraction::Background,
        };
        crate::app::turn::execute(&mut store, &req, sink)
    }));
    match outcome {
        Ok(Ok(())) => {}
        // execute 出错时已经往 sink 推过 error 事件了，这里只补服务端日志
        Ok(Err(e)) => eprintln!("会话 {sid} 这一轮失败: {e}"),
        Err(_) => {
            eprintln!("⚠️  会话 {sid} 这一轮 panic 了");
            sink.emit_json(&crate::wire::error_json(
                "分析过程内部出错了，这一轮没有答完。问题已经保存，可以再问一次。",
            ));
        }
    }
    match store.interrupt_running(Some(sid)) {
        Ok(0) => {}
        Ok(n) => eprintln!("会话 {sid} 有 {n} 轮没收完尾，已标成中断"),
        Err(e) => eprintln!("⚠️  会话 {sid} 收拾残局失败: {e}"),
    }
}

/// `GET /internal/sessions/{id}/events`
///
/// 接着看一个**正在跑**的提问：先把这一轮已经发过的事件从第一条开始补发，
/// 再接着实时推，直到 done / error。
///
/// 刷新页面、换一个标签页打开同一个会话，都走这里。
///
/// 404 两种：
///   `not_found`         不是你的会话 / 不存在（不区分，见 session_messages）
///   `no_running_turn`   是你的，但现在没有在跑的——多半是刚答完。
///                       前端看到它就去拉一遍聊天记录，答案已经在库里了。
async fn session_events(
    State(st): State<Arc<AppState>>,
    who: Principal,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    // 归属先查，**再**看有没有在跑。反过来的话，拿别人的会话 id 来问，
    // 「没在跑」和「不是你的」给出不同的回答，就能探出别人的会话在不在忙。
    let owns = {
        let st = Arc::clone(&st);
        let (uid, sid) = (who.user_id.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || st.read().session_owned_by(&uid, &sid))
            .await
            .map_err(join_failed)??
    };
    if !owns {
        return Err(not_found_session());
    }
    match st.registry.live(&session_id) {
        Some(live) => Ok(subscribe_sse(live)),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_running_turn",
            "这个会话现在没有在跑的提问",
        )),
    }
}

/// 订阅一轮的事件记录，变成一条 SSE 响应：从第一条补发，然后接着推。
///
/// ## 为什么每个订阅者单独一个转发任务
///
/// 事件记录本身**只管追加、从不等人**（agent 循环写它）。每个订阅者起一个
/// 小的 async 任务，按自己的进度（cursor）从记录里读，再塞进一个 64 深的
/// 队列交给 HTTP 响应。慢的浏览器只会让**它自己的**转发任务在
/// `send().await` 上等着，不影响 agent 循环，也不影响别的订阅者。
///
/// 浏览器断开 → HTTP 响应被丢掉 → 队列接收端没了 → `send` 报错 →
/// 转发任务退出。不会留下孤儿任务。
fn subscribe_sse(live: Arc<crate::app::live::LiveTurn>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
    tokio::spawn(async move {
        // ★ 先订阅「有新东西」的信号，再读记录。顺序反了的话，在「读完」和
        //   「开始等」之间来的那一条会被漏掉，这个订阅者就永远卡在等待上。
        let mut tick = live.watch();
        let mut cursor = 0usize;
        loop {
            let (batch, finished) = live.read_from(cursor);
            cursor += batch.len();
            for line in batch {
                if tx.send(line.to_string()).await.is_err() {
                    return; // 浏览器走了
                }
            }
            if finished {
                return;
            }
            if tick.changed().await.is_err() {
                // 事件记录被整个丢掉了（不该发生：订阅者手里有 Arc）。
                // 保险起见最后读一遍再走。
                let (batch, _) = live.read_from(cursor);
                for line in batch {
                    if tx.send(line.to_string()).await.is_err() {
                        return;
                    }
                }
                return;
            }
        }
    });

    // keep_alive 每 15 秒发一个 SSE 注释行（`:`）。一次提问里下载视频那段
    // 可能一分钟没有任何事件，中间的反向代理会把空闲连接掐掉。
    // 注释行不是 `data:` 开头，前端本来就跳过它。
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|line| {
        Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(line))
    });
    axum::response::sse::Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
        )
        .into_response()
}

#[derive(serde::Deserialize)]
struct TurnBody {
    /// 浏览器点发送时生成的编号。重试带同一个。
    request_id: String,
    /// 在哪个会话里问。不给（或空串）= 新会话里的第一个问题。
    session_id: Option<String>,
    question: String,
    /// 不给就按环境变量里配了谁的 key 自动挑。
    provider: Option<String>,
}

/// `POST /internal/sessions/{id}/turns/{turn}/cancel`
///
/// 停止这个会话里编号为 `turn` 的那一轮。编号从 hello 事件的 `turn` 字段来。
///
/// ```text
/// 正在跑，15 秒内停下了                200 {"turn":…, "status":"cancelled"}
/// 正在跑，模型说完的是最终答案         200 {"turn":…, "status":"done"}
/// 正在跑，15 秒还没停（模型还在说）    202 {"turn":…, "status":"cancelling"}
/// 早就结束了                           200 {"turn":…, "status": 真实结局}
/// 不是你的会话 / 这一轮不在这个会话里  404
/// ```
///
/// ## 为什么按「这一轮」停，不按会话停
///
/// 用户点了停止，请求在路上卡住了；这期间那一轮自己答完了，用户又发了
/// 新问题。按会话停的话，迟到的请求会把新问题停掉。按编号停：登记表里
/// 这个会话正在跑的已经是另一轮，对不上，什么都不做，返回旧那轮的结局。
///
/// ## 200 保证了什么
///
/// 这一轮**真的停了**：库里是终态，会话锁已经放开。前端收到就能马上在同一个
/// 会话里发下一个问题（Permit 的 Drop 先放锁、再标记事件记录结束，这里等的
/// 是后者）。重复调、网络重试都安全——第二次只是查一下结局。
///
/// 接口本身不改库：开关打开之后，「已停止」由循环自己写进去（Laplace 的
/// 规则：只有执行那一轮的循环能写它的终态）。
async fn cancel_turn(
    State(st): State<Arc<AppState>>,
    who: Principal,
    Path((session_id, turn_id)): Path<(String, String)>,
) -> std::result::Result<Response, ApiError> {
    // 归属先查。和 session_events 同一个道理：别人的会话在跑和没在跑
    // 必须给一样的回答。
    let owns = {
        let st = Arc::clone(&st);
        let (uid, sid) = (who.user_id.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || st.read().session_owned_by(&uid, &sid))
            .await
            .map_err(join_failed)??
    };
    if !owns {
        return Err(not_found_session());
    }

    if let Some(live) = st.registry.request_cancel(&session_id, &turn_id)
        && !wait_finished(&live, st.cancel_wait).await
    {
        return Ok((
            StatusCode::ACCEPTED,
            axum::Json(json!({"turn": turn_id, "status": "cancelling"})),
        )
            .into_response());
    }

    // 没在跑，或者刚停好：结局以库里为准。
    let status = {
        let st = Arc::clone(&st);
        let (sid, tid) = (session_id.clone(), turn_id.clone());
        tokio::task::spawn_blocking(move || st.read().turn_status(&sid, &tid))
            .await
            .map_err(join_failed)??
    };
    match status {
        Some(s) => Ok(axum::Json(json!({"turn": turn_id, "status": s.as_str()})).into_response()),
        // 编号不存在、或者属于别的会话——一样的回答。会话已经确认是你的，
        // 所以这里说不出任何关于别人的事。
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "这个会话里没有这一轮",
        )),
    }
}

/// 等这一轮的事件记录被标成结束，最多等 `limit`。等到了返回 true。
async fn wait_finished(live: &crate::app::live::LiveTurn, limit: std::time::Duration) -> bool {
    // 先订阅再看：反过来的话，「看完」和「开始等」之间结束的那一下会漏掉，
    // 白等到超时。（和 subscribe_sse 同一个道理。）
    let mut tick = live.watch();
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if live.is_finished() {
            return true;
        }
        match tokio::time::timeout_at(deadline, tick.changed()).await {
            Ok(Ok(())) => {}
            // 超时，或者发信号的那一头没了（只会在记录本身被扔掉时发生）
            Ok(Err(_)) | Err(_) => return live.is_finished(),
        }
    }
}

/// `GET /internal/memories`
///
/// 返回 `{"memories":[{id,category,content,evidence,created_at,updated_at}],
///        "limits":{"profile":3,"preference":10,"exclusion":50}}` —— 只有这个
/// 用户生效中的记忆。过滤在 SQL 的 WHERE 里（`SqliteStore::active_memories`）。
///
/// 上限一起给出去，面板上显示「偏好 8/10」用。上限只在 Rust 这边写一份
/// （`agent::memory::Category::cap`），前端不再抄一遍。
async fn list_memories(
    State(st): State<Arc<AppState>>,
    who: Principal,
) -> std::result::Result<Response, ApiError> {
    let ms = tokio::task::spawn_blocking(move || st.read().active_memories(&who.user_id))
        .await
        .map_err(join_failed)??;
    let limits: serde_json::Map<String, serde_json::Value> = crate::agent::memory::Category::ALL
        .into_iter()
        .map(|c| (c.as_str().to_string(), json!(c.cap())))
        .collect();
    Ok(axum::Json(json!({"memories": ms, "limits": limits})).into_response())
}

/// `DELETE /internal/memories/{id}`
///
/// 删掉自己的一条记忆，成功回 204。不存在、不是你的、已经删过，**一律 404**，
/// 不区分——理由同 `session_messages`。
///
/// 删掉之后，这个用户所有会话冻结的记忆段都会清掉、下一轮重新拼：删了就要
/// 马上不再影响回答（见 `SqliteStore::delete_memory`）。
async fn delete_memory(
    State(st): State<Arc<AppState>>,
    who: Principal,
    Path(id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let deleted = tokio::task::spawn_blocking(move || st.read().delete_memory(&who.user_id, &id))
        .await
        .map_err(join_failed)??;
    if deleted {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "没有这条记忆",
        ))
    }
}

/// 会话相关的「没找到」。消息刻意含糊：不存在和不是你的给一样的回答。
fn not_found_session() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "没有这个会话")
}

/// 列表接口的 query 参数。
#[derive(serde::Deserialize)]
struct ListQuery {
    limit: Option<usize>,
}

/// 不传 limit 时返回多少条。和改造前 Go 那边写死的 200 保持一致。
const DEFAULT_SESSION_LIMIT: usize = 200;
/// 传得再大也就到这儿。没有上限的话，一个 `?limit=99999999` 就能让服务
/// 去拼一个巨大的 JSON——不是攻击也可能是手滑。
const MAX_SESSION_LIMIT: usize = 500;

// ── 启动 ────────────────────────────────────────────────────

/// 起服务并一直跑，直到收到停机信号。`clipknow serve` 调它。
pub fn run(db_path: &str, addr: &str, max_turns: usize) -> Result<()> {
    if !(1..=MAX_TURNS_LIMIT).contains(&max_turns) {
        return Err(ClipKnowError::BadRequest(format!(
            "--max-turns 是 {max_turns}，得在 1 到 {MAX_TURNS_LIMIT} 之间。\n\
             上限卡在 blocking 线程池（{MAX_BLOCKING_THREADS} 条）减 2：每个在跑的提问占一条\n\
             好几分钟，全占满的话会话列表和健康检查就排不上队了。"
        )));
    }
    // open 只检查迁移版本，不执行 DDL——版本不对会在这里就报错并告诉人
    // 该跑 `clipknow migrate`，而不是等到第一次查询才炸。
    // ★ 凭证密钥在**开库之前**读。
    //   没配密钥是最常见的启动失败，而它和数据库一点关系都没有——排在后面
    //   的话，一个没迁移的库会先报「版本落后」，把真正的原因盖住。
    let verifier = auth::Verifier::from_env()?;
    let mut store = SqliteStore::open(db_path)?;
    // ★ 上一次进程没正常收尾（崩溃、kill -9）留下的「正在回答」，改成中断。
    //   不改的话它们会永远显示「正在回答」，前端还会不停地去订阅一个根本
    //   不存在的事件流。**不自动重跑**：外部调用（抓取、视频分析）可能已经
    //   花过钱了。在开始接受请求**之前**做，不会误伤这次启动后的新提问。
    let n = store.interrupt_running(None)?;
    if n > 0 {
        eprintln!("上次有 {n} 轮提问没答完服务就停了，已标成中断");
    }
    let state =
        Arc::new(AppState::new(store, db_path.to_string(), verifier).with_max_turns(max_turns));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_name("clipknow")
        .build()?;

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        warn_if_public(bound);
        // 人看的信息一律 stderr。stdout 留给 NDJSON（`clipknow turn` 那条路
        // 的不变量，见 wire.rs），这里不写 stdout 是为了不破坏它。
        eprintln!("ClipKnow agent 服务  →  http://{bound}");
        eprintln!("  库 {db_path}");
        eprintln!("  最多同时跑 {max_turns} 个提问（同一个会话仍然一次一个）");
        let reg = Arc::clone(&state.registry);
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async move { drain_then_stop(reg).await })
            .await?;
        eprintln!("已停止");
        Ok(())
    })
}

/// 绑到非回环地址时在 stderr 上喊一声。
///
/// 不直接拒绝，是因为 Docker 里两个容器要互相访问，必须绑 0.0.0.0。
/// 但开发机上绑错（比如手滑写成 `0.0.0.0:3100`）就等于把这个**不验证
/// 浏览器 Cookie、只认内部凭证**的服务挂到局域网上，值得一句显眼的警告。
fn warn_if_public(addr: SocketAddr) {
    if !addr.ip().is_loopback() {
        eprintln!(
            "⚠️  监听在 {} —— 不是回环地址，局域网里能直接访问。",
            addr.ip()
        );
        eprintln!("    这个服务不校验浏览器 Cookie，只认内部凭证；");
        eprintln!("    只有在容器网络这类受控环境里才该这么绑。");
    }
}

/// 等一个停机信号。
///
/// 两个都要接：`Ctrl-C`（开发机上手动停）和 `SIGTERM`（`docker stop` 和
/// systemd 发的那个）。只接 Ctrl-C 的话，容器会在 10 秒宽限期后被
/// `SIGKILL` 硬杀，正在跑的执行连落库的机会都没有。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            // 注册不上就永远不触发，让 Ctrl-C 那条路自己等
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// 在跑的执行最多等多久。
///
/// 一次提问本来就可能跑几分钟，硬杀掉等于把已经花掉的 SC 配额、模型 token
/// 和视频分析钱全扔了，而且那一轮不会落库。300 秒是给「正常的一轮跑完」
/// 留的余量；真等满说明它卡住了，那时候退出比继续等有用。
///
/// ⚠️ Docker 默认的停机宽限期是 **10 秒**，到点就 SIGKILL。要让这个等待
/// 真正生效，部署时得把 `stop_grace_period` 调到比这个数大。
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// 收到停机信号之后：先不收新的，再等在跑的跑完。
async fn drain_then_stop(registry: Arc<crate::app::registry::Registry>) {
    shutdown_signal().await;
    // 第一步：立刻停止接受新的提问。这一步是瞬时的，所以即使下面等满了，
    // 也不会有新的执行在这期间挤进来。
    registry.stop_accepting();
    let n = registry.running_count();
    if n == 0 {
        eprintln!("收到停机信号，没有在跑的执行，直接退出");
        return;
    }
    eprintln!("收到停机信号，不再接受新提问；等 {n} 个在跑的执行结束（最多 {DRAIN_TIMEOUT:?}）");

    // ★ wait_until_idle 是**阻塞**的（里面 sleep 轮询），不能直接在 async
    //   里调——会把一条 worker 线程占住整整 5 分钟。丢给 blocking 池。
    let left = tokio::task::spawn_blocking(move || registry.wait_until_idle(DRAIN_TIMEOUT))
        .await
        .unwrap_or(0);
    if left > 0 {
        eprintln!("⚠️  还有 {left} 个执行没跑完就被中断了，它们那一轮不会落库");
    } else {
        eprintln!("在跑的都收干净了");
    }
}

// ── 测试用的服务器 ──────────────────────────────────────────

#[cfg(test)]
pub(crate) mod testserver {
    use super::*;
    use std::sync::mpsc;

    /// 一个跑在后台线程上的真服务器。
    ///
    /// ★ 为什么是**真的监听端口**，而不是用 tower 的 `oneshot` 直接喂请求：
    ///   oneshot 绕过了 listener、HTTP 解析和响应序列化，而这三处正是
    ///   「本地全绿、一上线就 400」的高发地带。代价是多一个端口和一个线程，
    ///   一次测试几毫秒，买得起。
    ///
    /// ★ 为什么服务器在**另一条线程**上：测试线程用的是 `reqwest::blocking`
    ///   （项目已有的依赖，不用为测试再引一个 async HTTP 客户端），而在
    ///   tokio runtime 里调 blocking 客户端会直接 panic。分开两条线程，
    ///   两边都用自己最顺手的写法。
    pub struct TestServer {
        pub addr: SocketAddr,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        /// 服务器线程跑完 `block_on` 之后往这里发一声。
        /// 用它而不是 `JoinHandle::join()`，见 `Drop` 里的说明。
        done: mpsc::Receiver<()>,
    }

    impl TestServer {
        pub fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }
    }

    /// 等服务器线程收摊最多等多久。
    ///
    /// 正常停机是毫秒级；这个数只是用来把「卡死」变成「几秒后继续」。
    const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    impl Drop for TestServer {
        fn drop(&mut self) {
            // 发停机信号，然后等线程收摊。不等的话，一次 cargo test 会攒下
            // 几十个还在监听的端口，后面的测试随机撞端口失败。
            if let Some(tx) = self.stop.take() {
                let _ = tx.send(());
            }
            // ★ 这里**不能**用 `JoinHandle::join()`。
            //
            //   join 没有超时。优雅停机一旦写坏（实测：把
            //   `.with_graceful_shutdown(...)` 去掉），服务器线程永远不返回，
            //   于是整个测试**卡死**——CI 上那是六小时超时，不是一个红叉。
            //   故障注入的时候就是这么发现的。
            //
            //   换成「等一个完成通知，最多 5 秒」：坏掉时 5 秒后继续往下走，
            //   由测试自己的断言（端口还连得上 = 没停成）把它判红。
            //   判红这件事留给测试，不在 Drop 里 panic——Drop 里 panic
            //   会盖掉真正的失败原因，而且和别的 panic 撞上会直接 abort。
            if self.done.recv_timeout(STOP_TIMEOUT).is_err() {
                eprintln!(
                    "⚠️  测试服务器 {} 在 {STOP_TIMEOUT:?} 内没停下来",
                    self.addr
                );
            }
        }
    }

    /// 起一个监听随机空闲端口的服务器，等它真的在听了才返回。
    pub fn spawn(state: Arc<AppState>) -> TestServer {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        // 端口要从服务器线程回传给测试线程——bind 之前不知道系统分了哪个
        let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("测试 runtime 起不来");
            rt.block_on(async move {
                // 端口给 0 = 让系统挑一个空闲的。写死端口的话，
                // 并行跑的测试会互相撞。
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("绑不上端口");
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let _ = axum::serve(listener, router(state))
                    .with_graceful_shutdown(async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
            // 跑到这里说明 serve 真的返回了 = 停机成功。通知 Drop。
            let _ = done_tx.send(());
        });

        // recv 会一直等到服务器线程发来地址，那时候 listener 已经在听了——
        // 不需要 sleep 一个「应该够了吧」的时长。
        let addr = addr_rx.recv().expect("服务器线程没起来");
        TestServer {
            addr,
            stop: Some(stop_tx),
            done: done_rx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的密钥。够长，且和黄金样例那个不一样。
    const TEST_SECRET: &[u8] = b"serve-tests-secret-at-least-32-chars";

    fn state() -> Arc<AppState> {
        let store = SqliteStore::in_memory().expect("建内存库");
        Arc::new(AppState::new(
            store,
            ":memory:".into(),
            auth::Verifier::new(TEST_SECRET),
        ))
    }

    /// 签一张这个测试服务器认得的凭证。
    fn token_for(user_id: &str) -> String {
        use jsonwebtoken::{Algorithm, EncodingKey, Header};
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({
            "sub": user_id,
            "iss": auth::ISSUER,
            "aud": auth::AUDIENCE,
            "iat": now,
            "exp": now + 120,
        });
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(TEST_SECRET),
        )
        .unwrap()
    }

    #[test]
    fn health_报告库的真实迁移版本() {
        let srv = testserver::spawn(state());
        let r = reqwest::blocking::get(srv.url("/internal/health")).unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v["ok"], true);
        // 写死的常量会让这条测试永远通过。对着 migrate 里的期望版本比，
        // 以后加一条迁移忘了跑，这里就会红。
        assert_eq!(
            v["schema_version"],
            crate::store::migrate::expected_version(),
            "health 报的版本和代码期望的对不上: {v}"
        );
    }

    #[test]
    fn 不认识的路径回_404_而且是同一种错误形状() {
        let srv = testserver::spawn(state());
        let r = reqwest::blocking::get(srv.url("/internal/nope")).unwrap();
        assert_eq!(r.status(), 404);
        // 形状必须和别的错误一致：Go 那边只写一条解析路径。
        // 默认的 fallback 返回空 body，Go 解析会拿到一堆 null。
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v["code"], "not_found");
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
    }

    // ── 内部凭证在**真实 HTTP 请求**上的行为 ──────────────
    //
    // auth.rs 里那批单测直接调 verify()，走不到 header 解析和 axum 的
    // 提取器拒绝路径。下面这几条走完整条链：真 TCP → 真 HTTP 头 →
    // 提取器 → 响应码和响应体。

    #[test]
    fn 带对凭证能拿到自己的_user_id() {
        let srv = testserver::spawn(state());
        let r = reqwest::blocking::Client::new()
            .get(srv.url("/internal/whoami"))
            .bearer_auth(token_for("u_alice"))
            .send()
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<serde_json::Value>().unwrap()["user_id"], "u_alice");
    }

    #[test]
    fn 不带凭证的请求一律_401() {
        let srv = testserver::spawn(state());
        let r = reqwest::blocking::get(srv.url("/internal/whoami")).unwrap();
        assert_eq!(r.status(), 401);
        // 错误形状要和别的一致，Go 那边只写一条解析路径
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v["code"], "unauthorized");
    }

    /// 各种形状不对的 Authorization 头。
    ///
    /// 挨个列出来是因为**解析失败被当成「没带凭证」**和**被当成「带了个
    /// 空凭证」**是两回事，后者有可能走进一条把空串当合法身份的路径。
    #[test]
    fn 头的形状不对也是_401() {
        let srv = testserver::spawn(state());
        let c = reqwest::blocking::Client::new();
        let good = token_for("u_alice");
        for bad in [
            String::new(),                  // 空头
            "Bearer".into(),                // 只有关键字
            "Bearer ".into(),               // 关键字加空格，没有令牌
            format!("Basic {good}"),        // 认证方式不对
            format!("Bearer {good} extra"), // 后面多了东西
            format!("Bearer {good}x"),      // 签名被改了一个字符
            "Bearer not.a.jwt".into(),
        ] {
            let r = c
                .get(srv.url("/internal/whoami"))
                .header("authorization", &bad)
                .send()
                .unwrap();
            assert_eq!(r.status(), 401, "这个头本该被拒: {bad:?}");
        }
    }

    /// ★ 换个密钥签的凭证必须被拒。
    ///
    /// 这条钉的是**两边密钥配错时的表现**：401，不是「校验没跑」。
    /// 校验没跑的表现是 200，而 200 意味着任何人都能冒充任意用户。
    #[test]
    fn 别的密钥签的凭证被拒() {
        use jsonwebtoken::{Algorithm, EncodingKey, Header};
        let srv = testserver::spawn(state());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 除了密钥，其它字段全对——所以拒绝的原因只能是签名
        let forged = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &json!({
                "sub": "u_alice", "iss": auth::ISSUER, "aud": auth::AUDIENCE,
                "iat": now, "exp": now + 120,
            }),
            &EncodingKey::from_secret(b"a-completely-different-32-byte-key!!!"),
        )
        .unwrap();
        let r = reqwest::blocking::Client::new()
            .get(srv.url("/internal/whoami"))
            .bearer_auth(forged)
            .send()
            .unwrap();
        assert_eq!(r.status(), 401);
    }

    /// health 不设防 —— Docker 的 healthcheck 没有密钥可用。
    #[test]
    fn health_不需要凭证() {
        let srv = testserver::spawn(state());
        assert_eq!(
            reqwest::blocking::get(srv.url("/internal/health"))
                .unwrap()
                .status(),
            200
        );
    }

    // ── 会话读接口 ────────────────────────────────────────

    /// 造两个用户，各一个会话，阿的那个里有一轮对话。
    /// 返回 (state, 阿的会话 id)。
    fn two_users() -> (Arc<AppState>, String) {
        let (st, sid) = two_users_state();
        (Arc::new(st), sid)
    }

    /// 同 [`two_users`]，但还没包进 Arc，测试能先改配置（比如取消的等待上限）。
    fn two_users_state() -> (AppState, String) {
        use crate::content::model::{Item, ItemKind, TurnStatus};
        let mut store = SqliteStore::in_memory().expect("建内存库");
        for id in ["u_alice", "u_bob"] {
            store
                .exec_for_test(&format!(
                    "INSERT INTO users (id, username_normalized, display_name, password_hash,
                                        status, created_at, updated_at)
                     VALUES ('{id}','{id}','{id}','x','active',0,0)"
                ))
                .unwrap();
        }
        let sid = store
            .create_session(Some("阿的会话"), Some("u_alice"))
            .unwrap();
        store
            .create_session(Some("波的会话"), Some("u_bob"))
            .unwrap();
        store
            .save_turn(
                &sid,
                "fake",
                TurnStatus::Done,
                &[
                    Item {
                        idx: 0,
                        kind: ItemKind::UserMessage,
                        iteration: None,
                        call_id: None,
                        payload: json!({"text": "阿问的问题"}),
                        raw_json: None,
                    },
                    Item {
                        idx: 1,
                        kind: ItemKind::AssistantMessage,
                        iteration: Some(1),
                        call_id: None,
                        payload: json!({"text": "给阿的答案"}),
                        raw_json: None,
                    },
                ],
            )
            .unwrap();
        (
            AppState::new(store, ":memory:".into(), auth::Verifier::new(TEST_SECRET)),
            sid,
        )
    }

    /// 带凭证发一个 GET。
    fn get_as(srv: &testserver::TestServer, user: &str, path: &str) -> reqwest::blocking::Response {
        reqwest::blocking::Client::new()
            .get(srv.url(path))
            .bearer_auth(token_for(user))
            .send()
            .unwrap()
    }

    #[test]
    fn 会话列表只出自己的() {
        let (st, _) = two_users();
        let srv = testserver::spawn(st);

        let v: serde_json::Value = get_as(&srv, "u_alice", "/internal/sessions")
            .json()
            .unwrap();
        let ss = v["sessions"].as_array().unwrap();
        assert_eq!(ss.len(), 1, "{v}");
        assert_eq!(ss[0]["title"], "阿的会话");

        let v: serde_json::Value = get_as(&srv, "u_bob", "/internal/sessions").json().unwrap();
        assert_eq!(v["sessions"][0]["title"], "波的会话");
    }

    // ── 长期记忆面板 ──────────────────────────────────────

    /// 两个用户各有记忆。返回 (state, 阿的那条记忆的 id)。
    fn two_users_with_memories() -> (Arc<AppState>, String) {
        use crate::agent::memory::{Category, MemoryOp};
        let (st, _) = two_users_state();
        let add = |c: Category, s: &str| MemoryOp::Add {
            category: c,
            content: s.into(),
            evidence: s.into(),
        };
        st.read()
            .apply_memory_ops(
                "u_alice",
                None,
                &[
                    add(Category::Preference, "阿只要中文博主"),
                    add(Category::Profile, "阿做美妆"),
                ],
            )
            .unwrap();
        st.read()
            .apply_memory_ops("u_bob", None, &[add(Category::Preference, "波的偏好")])
            .unwrap();
        let alice = st.read().active_memories("u_alice").unwrap()[1].id.clone();
        (Arc::new(st), alice)
    }

    fn delete_as(
        srv: &testserver::TestServer,
        user: &str,
        path: &str,
    ) -> reqwest::blocking::Response {
        reqwest::blocking::Client::new()
            .delete(srv.url(path))
            .bearer_auth(token_for(user))
            .send()
            .unwrap()
    }

    #[test]
    fn 记忆列表只出自己的_带着各类上限() {
        let (st, _) = two_users_with_memories();
        let srv = testserver::spawn(st);

        let v: serde_json::Value = get_as(&srv, "u_alice", "/internal/memories")
            .json()
            .unwrap();
        let ms = v["memories"].as_array().unwrap();
        assert_eq!(ms.len(), 2, "{v}");
        // 身份排在偏好前面
        assert_eq!(ms[0]["category"], "profile");
        assert_eq!(ms[0]["content"], "阿做美妆");
        assert!(
            ms[0]["evidence"].is_string() && ms[0]["id"].is_string(),
            "{v}"
        );
        assert!(!v.to_string().contains("波的偏好"), "混进了别人的记忆：{v}");
        assert_eq!(v["limits"]["exclusion"], 50, "{v}");

        let r = reqwest::blocking::get(srv.url("/internal/memories")).unwrap();
        assert_eq!(r.status(), 401, "不带凭证不能看");
    }

    #[test]
    fn 删自己的记忆是_204_再删一次和删别人的都是_404() {
        let (st, alice_mem) = two_users_with_memories();
        let srv = testserver::spawn(Arc::clone(&st));
        let path = format!("/internal/memories/{alice_mem}");

        // 波拿阿的记忆 id 来删：404，而且阿的那条还在
        let r = delete_as(&srv, "u_bob", &path);
        assert_eq!(r.status(), 404);
        assert_eq!(r.json::<serde_json::Value>().unwrap()["code"], "not_found");
        assert_eq!(st.read().active_memories("u_alice").unwrap().len(), 2);

        assert_eq!(delete_as(&srv, "u_alice", &path).status(), 204);
        assert_eq!(st.read().active_memories("u_alice").unwrap().len(), 1);
        assert_eq!(delete_as(&srv, "u_alice", &path).status(), 404, "删第二次");
        assert_eq!(
            st.read().active_memories("u_bob").unwrap().len(),
            1,
            "别人的不该被动"
        );
    }

    #[test]
    fn 自己会话的聊天记录读得到() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let r = get_as(
            &srv,
            "u_alice",
            &format!("/internal/sessions/{sid}/messages"),
        );
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().unwrap();
        let ms = v["messages"].as_array().unwrap();
        assert_eq!(ms.len(), 2, "{v}");
        assert_eq!(ms[0]["role"], "user");
        assert_eq!(ms[1]["text"], "给阿的答案");
        assert_eq!(ms[1]["failed"], false);
    }

    /// ★ 越权读必须是 404，而且**响应体里不能有对方的任何内容**。
    ///
    /// 只断言状态码是不够的：403 加一句「这是阿的会话」同样是 403，
    /// 而那句话本身就泄漏了。这里把整个响应体抓出来搜。
    #[test]
    fn 读别人的会话是_404_且不泄漏任何内容() {
        let (st, alice_sid) = two_users();
        let srv = testserver::spawn(st);
        let r = get_as(
            &srv,
            "u_bob",
            &format!("/internal/sessions/{alice_sid}/messages"),
        );
        assert_eq!(r.status(), 404);
        let body = r.text().unwrap();
        for leak in ["阿的会话", "阿问的问题", "给阿的答案", "u_alice"] {
            assert!(!body.contains(leak), "响应体里泄漏了 {leak:?}: {body}");
        }
    }

    #[test]
    fn 不存在的会话也是_404() {
        let (st, _) = two_users();
        let srv = testserver::spawn(st);
        assert_eq!(
            get_as(&srv, "u_alice", "/internal/sessions/根本没有/messages").status(),
            404
        );
    }

    #[test]
    fn 会话接口不带凭证一律_401() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        for path in [
            "/internal/sessions".to_string(),
            format!("/internal/sessions/{sid}/messages"),
        ] {
            let r = reqwest::blocking::get(srv.url(&path)).unwrap();
            assert_eq!(r.status(), 401, "{path} 居然不用凭证就能读");
        }
    }

    /// limit 要能被夹住。`?limit=99999999` 不该让服务去拼一个巨大的 JSON。
    #[test]
    fn limit_超范围会被夹住而不是报错() {
        let (st, _) = two_users();
        let srv = testserver::spawn(st);
        for q in ["?limit=0", "?limit=99999999", "?limit=1"] {
            let r = get_as(&srv, "u_alice", &format!("/internal/sessions{q}"));
            assert_eq!(r.status(), 200, "limit={q} 时报错了");
        }
        // 非法的 limit（不是数字）是调用方的错，该 400 而不是 500
        let r = get_as(&srv, "u_alice", "/internal/sessions?limit=abc");
        assert_eq!(r.status(), 400);
    }

    // ── 提问接口：流开始**之前**的那些拒绝 ────────────────
    //
    // 正常路径（真跑一轮）在 Go 那边的端到端测试里，那边有假模型。
    // 这里只测拒绝和去重——它们全都在 spawn_blocking 之前返回，不需要模型。

    fn post_turn(
        srv: &testserver::TestServer,
        user: &str,
        body: &str,
    ) -> reqwest::blocking::Response {
        reqwest::blocking::Client::new()
            .post(srv.url("/internal/turns"))
            .bearer_auth(token_for(user))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .unwrap()
    }

    /// 在 `session` 里问 `q`，提交编号 `rid`。`session` 为空 = 新会话。
    fn ask_body(rid: &str, session: &str, q: &str) -> String {
        json!({"request_id": rid, "session_id": session, "question": q}).to_string()
    }

    fn sessions_of(st: &AppState, user: &str) -> i64 {
        st.read()
            .count_for_test(&format!(
                "SELECT count(*) FROM sessions WHERE user_id = '{user}'"
            ))
            .unwrap()
    }

    /// ★ 拿别人的会话 id 来提问：404，而且**不占用执行名额**、不留任何记录。
    ///
    /// 名额的申请必须排在归属校验后面。反过来的话，随便拼一串别人的会话 id
    /// 狂发请求就能把名额占光——一个不用登录成别人就能做到的拒绝服务。
    #[test]
    fn 拿别人的会话提问是_404_且不占名额() {
        let (st, alice_sid) = two_users();
        let reg = Arc::clone(&st.registry);
        let srv = testserver::spawn(Arc::clone(&st));

        let r = post_turn(&srv, "u_bob", &ask_body("r1", &alice_sid, "偷看"));
        assert_eq!(r.status(), 404);
        assert_eq!(reg.running_count(), 0, "被拒的请求占住了执行名额");
        assert_eq!(
            st.read()
                .count_for_test("SELECT count(*) FROM chat_requests")
                .unwrap(),
            0,
            "被拒的请求留下了提交记录"
        );
    }

    #[test]
    fn 对不存在的会话提问是_404() {
        let (st, _) = two_users();
        let srv = testserver::spawn(st);
        assert_eq!(
            post_turn(&srv, "u_alice", &ask_body("r1", "根本没有", "喂")).status(),
            404
        );
    }

    #[test]
    fn 空问题是_400() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        for q in ["", "   "] {
            assert_eq!(
                post_turn(&srv, "u_alice", &ask_body("r1", &sid, q)).status(),
                400,
                "{q:?}"
            );
        }
    }

    /// 提交编号要进库、进主键：限长、限字符。
    #[test]
    fn 提交编号缺了或格式不对是_400() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let long = "a".repeat(65);
        for rid in ["", "带中文", "a b", "../x", long.as_str()] {
            assert_eq!(
                post_turn(&srv, "u_alice", &ask_body(rid, &sid, "喂")).status(),
                400,
                "{rid:?}"
            );
        }
        let missing = json!({"session_id": sid, "question": "喂"}).to_string();
        assert_eq!(post_turn(&srv, "u_alice", &missing).status(), 422);
    }

    /// provider 来自浏览器，是外部输入。必须走白名单。
    #[test]
    fn 不认识的_provider_是_400_但空的当作没给() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let bad = json!({"request_id":"r1","session_id":sid,"question":"喂","provider":"--db"});
        assert_eq!(post_turn(&srv, "u_alice", &bad.to_string()).status(), 400);

        // ★ 空串当作「没给」，不能变成「不认识的 provider: 」。
        //   传空串是调用方最容易犯的错（Go 那边就犯过一次），而报错
        //   「不认识的 provider: 」跟真实原因一个字都不沾。
        let empty = json!({"request_id":"r2","session_id":sid,"question":"喂","provider":""});
        let empty = post_turn(&srv, "u_alice", &empty.to_string());
        assert_ne!(
            empty.status(),
            400,
            "空 provider 被当成了非法值：{}",
            empty.text().unwrap()
        );
    }

    #[test]
    fn 提问接口不带凭证是_401() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let r = reqwest::blocking::Client::new()
            .post(srv.url("/internal/turns"))
            .header("content-type", "application/json")
            .body(ask_body("r1", &sid, "喂"))
            .send()
            .unwrap();
        assert_eq!(r.status(), 401);
    }

    /// 新会话里的第一个问题：会话由这个接口建，归属在建的那一刻就写进去。
    ///
    /// 名额设成 0，让请求停在申请名额那一步（503）——会话和提交记录在那之前
    /// 已经写好了，这里不用真跑模型。
    #[test]
    fn 新会话里提问_会话归属在创建那一刻写进去() {
        let (st, _) = two_users_state();
        let st = Arc::new(st.with_max_turns(0));
        let srv = testserver::spawn(Arc::clone(&st));
        let (bob0, alice0) = (sessions_of(&st, "u_bob"), sessions_of(&st, "u_alice"));

        let r = post_turn(&srv, "u_bob", &ask_body("r1", "", "新会话的问题"));
        assert_eq!(r.status(), 503, "{}", r.text().unwrap());
        assert_eq!(
            sessions_of(&st, "u_bob"),
            bob0 + 1,
            "会话没建，或者建在了别人名下"
        );
        assert_eq!(sessions_of(&st, "u_alice"), alice0);
    }

    // ── 请求幂等 ───────────────────────────────────────────

    /// ★ 新会话里提问被拒（名额满），浏览器带同一个编号重试：还是那一个会话。
    ///
    ///   这就是把「建会话」和「提问」合成一个接口要解决的场景。以前 Go 分两次
    ///   调，重试时会再建一个会话。
    #[test]
    fn 新会话里的提问重试_只建一个会话() {
        let (st, _) = two_users_state();
        let st = Arc::new(st.with_max_turns(0));
        let srv = testserver::spawn(Arc::clone(&st));
        let before = sessions_of(&st, "u_bob");
        for _ in 0..3 {
            let r = post_turn(&srv, "u_bob", &ask_body("r1", "", "问"));
            assert_eq!(r.status(), 503);
        }
        assert_eq!(sessions_of(&st, "u_bob"), before + 1, "重试又建了会话");
    }

    #[test]
    fn 同一个编号换了问题是_409() {
        let (st, sid) = two_users_state();
        let st = Arc::new(st.with_max_turns(0));
        let srv = testserver::spawn(Arc::clone(&st));
        assert_eq!(
            post_turn(&srv, "u_alice", &ask_body("r1", &sid, "问 A")).status(),
            503
        );
        let (code, v) = json_of(post_turn(&srv, "u_alice", &ask_body("r1", &sid, "问 B")));
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["code"], "request_conflict");
    }

    /// 已经跑完的那一轮：同一个编号再来，告诉它就是那一轮，不重跑。
    #[test]
    fn 跑完之后同一个编号再来_返回那一轮不重跑() {
        let (st, sid) = two_users();
        let tid = crate::content::model::new_id();
        {
            let mut db = st.read();
            db.claim_chat_request("u_alice", "r1", Some(&sid), "问")
                .unwrap();
            db.begin_turn(&tid, &sid, "m", "问", "问", Some(("u_alice", "r1")))
                .unwrap();
            db.finish_turn(&tid, &sid, crate::content::model::TurnStatus::Done, &[])
                .unwrap();
        }
        let srv = testserver::spawn(Arc::clone(&st));
        let r = post_turn(&srv, "u_alice", &ask_body("r1", &sid, "问"));
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "重复的提交该回 JSON，不是事件流"
        );
        let (code, v) = json_of(r);
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["duplicate"], true);
        assert_eq!(v["turn"], tid.as_str());
        assert_eq!(v["session"], sid.as_str());
        assert_eq!(v["status"], "done");
        assert_eq!(st.registry.running_count(), 0, "重复的提交又跑了一轮");
    }

    /// ★ 拿到名额、还没写库的那一小段：库里查不到，靠登记表认出来。
    #[test]
    fn 正在跑还没写库时同一个编号再来_返回那一轮() {
        let (st, sid) = two_users();
        st.read()
            .claim_chat_request("u_alice", "r1", Some(&sid), "问")
            .unwrap();
        let p = st.registry.admit(&sid, Some("r1")).unwrap();
        let srv = testserver::spawn(Arc::clone(&st));
        let (code, v) = json_of(post_turn(&srv, "u_alice", &ask_body("r1", &sid, "问")));
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["turn"], p.turn_id());
        assert_eq!(v["status"], "running");

        // 换一个编号就是另一次提交：照旧会话忙
        let (code, v) = json_of(post_turn(&srv, "u_alice", &ask_body("r2", &sid, "问")));
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["code"], "session_busy");
    }

    #[test]
    fn health_会报现在有几个在跑() {
        let (st, _) = two_users();
        let reg = Arc::clone(&st.registry);
        let srv = testserver::spawn(st);

        let v: serde_json::Value = reqwest::blocking::get(srv.url("/internal/health"))
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(v["running_turns"], 0);
        assert_eq!(v["max_turns"], crate::app::registry::DEFAULT_MAX_TURNS);
        assert_eq!(v["accepting"], true);

        let _p = reg.admit("s-x", None).unwrap();
        let v: serde_json::Value = reqwest::blocking::get(srv.url("/internal/health"))
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(v["running_turns"], 1, "health 报的在跑数不对: {v}");
    }

    /// 并发上限必须给查询留出 blocking 线程。
    ///
    /// 提问会把一条 blocking 线程占住好几分钟；上限等于池子大小的话，
    /// 所有提问一起跑时会话列表和健康检查就排不上队了。
    #[test]
    fn 并发上限给查询留了线程() {
        const { assert!(MAX_TURNS_LIMIT < MAX_BLOCKING_THREADS) };
        const { assert!(crate::app::registry::DEFAULT_MAX_TURNS <= MAX_TURNS_LIMIT) };
    }

    // ── 接着看：GET /internal/sessions/{id}/events ────────

    /// 读一条 SSE 响应里的全部 data 行（直到流结束）。
    fn sse_data_lines(r: reqwest::blocking::Response) -> Vec<serde_json::Value> {
        r.text()
            .unwrap()
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// ★ 刷新后重新连上：从第一条补发，接着推到结束。
    ///
    /// 直接往登记表里塞一轮假的在跑的提问，手工往它的事件记录里写事件，
    /// 不需要真的跑模型。
    #[test]
    fn 接着看会从第一条补发然后推到结束() {
        let (st, sid) = two_users();
        let reg = Arc::clone(&st.registry);
        let srv = testserver::spawn(st);

        let permit = reg.admit(&sid, None).unwrap();
        let live = permit.live();
        live.emit_json(&json!({"t": "hello", "session": sid}));
        live.emit_json(&json!({"t": "token", "text": "前半句"}));

        // 订阅者连上的时候，前两条已经发过了；之后再来两条，然后结束。
        let h = {
            let live = Arc::clone(&live);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                live.emit_json(&json!({"t": "token", "text": "后半句"}));
                live.emit_json(&json!({"t": "done", "outcome": "done", "note": ""}));
                drop(permit); // 名额归还 → 事件记录收尾
            })
        };
        let r = get_as(&srv, "u_alice", &format!("/internal/sessions/{sid}/events"));
        assert_eq!(r.status(), 200);
        let evs = sse_data_lines(r);
        h.join().unwrap();

        let ts: Vec<_> = evs
            .iter()
            .map(|v| v["t"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ts,
            vec!["hello", "token", "token", "done"],
            "补发或续播少了东西"
        );
        assert_eq!(evs[1]["text"], "前半句", "连上之前发过的没补上");
        assert_eq!(evs[2]["text"], "后半句", "连上之后发的没推过来");
    }

    #[test]
    fn 没在跑时接着看是_404_no_running_turn() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let r = get_as(&srv, "u_alice", &format!("/internal/sessions/{sid}/events"));
        assert_eq!(r.status(), 404);
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v["code"], "no_running_turn");
    }

    /// ★ 别人的会话：不管在不在跑，都是一模一样的 not_found。
    ///
    /// 要是「在跑」和「没在跑」给出不同的回答，拿别人的会话 id 来探，
    /// 就能知道别人现在在不在问问题。
    #[test]
    fn 别人的会话接着看一律_not_found_不泄露在不在跑() {
        let (st, alice_sid) = two_users();
        let reg = Arc::clone(&st.registry);
        let srv = testserver::spawn(st);
        let url = format!("/internal/sessions/{alice_sid}/events");

        let idle: serde_json::Value = get_as(&srv, "u_bob", &url).json().unwrap();
        let _p = reg.admit(&alice_sid, None).unwrap();
        let busy = get_as(&srv, "u_bob", &url);
        assert_eq!(busy.status(), 404);
        let busy: serde_json::Value = busy.json().unwrap();
        assert_eq!(idle, busy, "在跑和没在跑给了不同的回答");
        assert_eq!(busy["code"], "not_found");
    }

    #[test]
    fn 会话列表带着在跑的标记() {
        let (st, sid) = two_users();
        // 测试里的库是内存库，只有 AppState 手里那一条连接能看到它
        st.read()
            .begin_turn(&crate::content::model::new_id(), &sid, "m", "q", "q", None)
            .unwrap();
        let srv = testserver::spawn(st);
        let v: serde_json::Value = get_as(&srv, "u_alice", "/internal/sessions")
            .json()
            .unwrap();
        assert_eq!(v["sessions"][0]["running"], true, "{v}");
    }

    /// 停机信号发出后，服务器线程必须真的收摊。
    ///
    /// 钉这条是因为「优雅停机」写错了的表现是**进程退不掉**——
    /// 容器 stop 要等 10 秒宽限期然后被 SIGKILL，而日志上什么都看不出来。
    #[test]
    fn 停机之后端口就不再接受连接了() {
        let srv = testserver::spawn(state());
        let url = srv.url("/internal/health");
        assert!(reqwest::blocking::get(&url).is_ok());
        drop(srv); // Drop 里发停机信号并 join
        assert!(
            reqwest::blocking::get(&url).is_err(),
            "停机之后还能连上，说明 graceful shutdown 没生效"
        );
    }

    // ── 取消 ───────────────────────────────────────────────

    fn cancel_as(
        srv: &testserver::TestServer,
        user: &str,
        sid: &str,
        turn: &str,
    ) -> reqwest::blocking::Response {
        reqwest::blocking::Client::new()
            .post(srv.url(&format!("/internal/sessions/{sid}/turns/{turn}/cancel")))
            .bearer_auth(token_for(user))
            .send()
            .unwrap()
    }

    fn json_of(r: reqwest::blocking::Response) -> (u16, serde_json::Value) {
        let code = r.status().as_u16();
        (code, r.json().unwrap())
    }

    /// 在登记表里占一个名额、在库里写一行 running，假装这一轮在跑。
    /// 不真的跑循环：这里测的是接口，循环的行为在 runner 的测试里。
    fn fake_running(st: &AppState, sid: &str) -> crate::app::registry::Permit {
        let p = st.registry.admit(sid, None).unwrap();
        st.read()
            .begin_turn(p.turn_id(), sid, "m", "问", "问", None)
            .unwrap();
        p
    }

    #[test]
    fn 取消_别人的会话一律_404() {
        let (st, sid) = two_users();
        let p = fake_running(&st, &sid);
        let srv = testserver::spawn(Arc::clone(&st));
        let (code, v) = json_of(cancel_as(&srv, "u_bob", &sid, p.turn_id()));
        assert_eq!(code, 404, "{v}");
        assert!(!p.control().cancel.is_cancelled(), "别人能停掉阿的提问");
    }

    #[test]
    fn 取消_这个会话里没有这一轮_404() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let (code, v) = json_of(cancel_as(&srv, "u_alice", &sid, "no-such-turn"));
        assert_eq!(code, 404, "{v}");
        assert_eq!(v["code"], "not_found");
    }

    /// 早就结束了：返回真实结局，什么都不改。重复调也一样。
    #[test]
    fn 取消_已经结束的一轮返回真实结局() {
        let (st, sid) = two_users();
        let tid = crate::content::model::new_id();
        st.read()
            .begin_turn(&tid, &sid, "m", "问", "问", None)
            .unwrap();
        st.read()
            .finish_turn(&tid, &sid, crate::content::model::TurnStatus::Done, &[])
            .unwrap();
        let srv = testserver::spawn(st);
        for _ in 0..2 {
            let (code, v) = json_of(cancel_as(&srv, "u_alice", &sid, &tid));
            assert_eq!(code, 200, "{v}");
            assert_eq!(v["status"], "done");
            assert_eq!(v["turn"], tid.as_str());
        }
    }

    /// ★ 正在跑：开关打开 → 循环写下终态、放开会话锁 → 接口返回 200。
    ///   返回之后**马上**在同一个会话申请，必须拿得到。
    #[test]
    fn 取消_正在跑的一轮停下后返回_cancelled_会话马上能再问() {
        let (st, sid) = two_users();
        let st = Arc::new(st);
        let p = fake_running(&st, &sid);
        let tid = p.turn_id().to_string();
        let ctl = p.control();

        // 假装是循环：看到开关打开，写下终态，结束这一轮
        let (st2, sid2, tid2) = (Arc::clone(&st), sid.clone(), tid.clone());
        let looper = std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            while !ctl.cancel.is_cancelled() {
                assert!(t0.elapsed().as_secs() < 5, "开关一直没被打开");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            st2.read()
                .finish_turn(
                    &tid2,
                    &sid2,
                    crate::content::model::TurnStatus::Cancelled,
                    &[],
                )
                .unwrap();
            drop(p);
        });

        let srv = testserver::spawn(Arc::clone(&st));
        let (code, v) = json_of(cancel_as(&srv, "u_alice", &sid, &tid));
        looper.join().unwrap();
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["status"], "cancelled");
        assert!(
            st.registry.admit(&sid, None).is_ok(),
            "接口说停好了，同一个会话却还不能再问"
        );
    }

    /// 等满了还没停（模型还在说话）：202「正在收尾」，开关已经打开。
    #[test]
    fn 取消_等满了还没停返回_202() {
        let (st, sid) = two_users_state();
        let st = Arc::new(st.with_cancel_wait(std::time::Duration::from_millis(200)));
        let p = fake_running(&st, &sid);
        let srv = testserver::spawn(Arc::clone(&st));

        let (code, v) = json_of(cancel_as(&srv, "u_alice", &sid, p.turn_id()));
        assert_eq!(code, 202, "{v}");
        assert_eq!(v["status"], "cancelling");
        assert!(p.control().cancel.is_cancelled(), "202 也得把开关打开");
    }

    /// ★ 迟到的取消：拿上一轮的编号来，停不掉正在跑的新一轮。
    #[test]
    fn 取消_拿上一轮的编号停不掉新的一轮() {
        let (st, sid) = two_users();
        let old = crate::content::model::new_id();
        st.read()
            .begin_turn(&old, &sid, "m", "旧", "旧", None)
            .unwrap();
        st.read()
            .finish_turn(&old, &sid, crate::content::model::TurnStatus::Done, &[])
            .unwrap();
        let new = fake_running(&st, &sid);
        let srv = testserver::spawn(Arc::clone(&st));

        let (code, v) = json_of(cancel_as(&srv, "u_alice", &sid, &old));
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["status"], "done", "该返回旧那一轮的结局");
        assert!(
            !new.control().cancel.is_cancelled(),
            "迟到的取消请求把新的一轮停了"
        );
    }

    #[test]
    fn 健康检查报名额用了几个() {
        let (st, sid) = two_users();
        let _p = st.registry.admit(&sid, None).unwrap();
        let srv = testserver::spawn(st);
        let v: serde_json::Value = reqwest::blocking::get(srv.url("/internal/health"))
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(v["slots_in_use"], 1);
        assert_eq!(v["running_turns"], 1);
    }
}

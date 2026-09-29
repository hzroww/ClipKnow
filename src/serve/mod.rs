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
use axum::routing::{get, post};
use serde_json::json;
use tokio_stream::StreamExt;

use crate::error::{ClipKnowError, Result};
use crate::store::sqlite::SqliteStore;
use crate::wire::TurnSink;

pub mod auth;
pub mod sink;
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
    /// 阶段 B 的策略是「全局最多 1 个」，和改造前 Go 那把 turnGate 一样。
    pub registry: Arc<crate::app::registry::Registry>,
}

impl AppState {
    pub fn new(store: SqliteStore, db_path: String, verifier: auth::Verifier) -> Self {
        AppState {
            verifier,
            registry: crate::app::registry::Registry::new(),
            read: Mutex::new(store),
            db_path,
        }
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
        // 往下都要内部凭证。靠 handler 参数里的 `Principal` 提取器强制，
        // 不是靠中间件——见 auth.rs 里关于「忘了调」的说明。
        .route("/internal/whoami", get(whoami))
        .route(
            "/internal/sessions",
            get(list_sessions).post(create_session),
        )
        // axum 0.8 的路径参数是 {id}，不是老版本的 :id
        .route("/internal/sessions/{id}/messages", get(session_messages))
        .route("/internal/sessions/{id}/turns", post(create_turn))
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
    let (running, accepting) = (st.registry.running_count(), st.registry.is_accepting());
    let version = tokio::task::spawn_blocking(move || st.read().schema_version())
        .await
        .map_err(join_failed)??;
    Ok(axum::Json(json!({
        "ok": true,
        "schema_version": version,
        "running_turns": running,
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

/// `POST /internal/sessions`
///
/// 请求体 `{"title": null}`，返回 `201 {"session_id":"..."}`。
///
/// ★ 归属在**创建那一刻**就写进 `sessions.user_id`，不是事后认领。
///   这是设计文档第 12 节的第 2 条不变量。改造前的做法是 Rust 先建会话、
///   把 id 放在 hello 那一行报给 Go、Go 再回头认领一次——中间那一小段时间
///   里会话是无主的，而且认领失败（进程被杀）就永远无主了。
async fn create_session(
    State(st): State<Arc<AppState>>,
    who: Principal,
    body: Option<axum::Json<CreateSessionBody>>,
) -> std::result::Result<Response, ApiError> {
    // body 可有可无：`POST` 一个空请求体就是「建一个没标题的会话」。
    let title = body.and_then(|axum::Json(b)| b.title);
    let id = tokio::task::spawn_blocking(move || {
        crate::app::sessions::create(&mut st.read(), &who.user_id, title.as_deref())
    })
    .await
    .map_err(join_failed)??;
    Ok((StatusCode::CREATED, axum::Json(json!({"session_id": id}))).into_response())
}

#[derive(serde::Deserialize)]
struct CreateSessionBody {
    title: Option<String>,
}

/// `POST /internal/sessions/{id}/turns`
///
/// 请求体 `{"question":"...","provider":"deepseek"}`。
/// 成功返回 `200 text/event-stream`，事件体和改造前 `clipknow turn` 打到
/// stdout 的 NDJSON **一模一样**——前端一个字都不用改。
///
/// ## 流开始之前的拒绝走 HTTP 状态码
///
/// 400 参数错 / 404 不是你的会话 / 409 这个会话在跑 / 503 全局名额满。
/// 设计文档第 6.2 节：「HTTP 头发出前的拒绝使用 HTTP 状态码；开始流之后
/// 用事件报告错误」。混着来的话，Go 和前端都得同时处理两套错误路径。
///
/// ## 浏览器断了怎么办
///
/// **什么都不做，执行照常跑完并落库。** 那一轮已经花了 SC 配额、模型 token、
/// 可能还有一次视频分析的钱，为了「你关了页面」把这些扔掉是最亏的。
/// 事件发不出去时 [`sink::ChannelSink`] 会丢掉它们，循环不受影响。
async fn create_turn(
    State(st): State<Arc<AppState>>,
    who: Principal,
    Path(session_id): Path<String>,
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
    // provider 来自浏览器，是外部输入。parse_provider 只认白名单里那两个，
    // 拼错直接报错而不是悄悄用默认那家。
    let provider = crate::app::turn::parse_provider(body.provider.as_deref())?;

    // ★ 归属校验在申请名额**之前**。
    //   反过来的话，拿别人的会话 id 乱发请求就能把名额占掉——
    //   一个不用登录成别人就能做到的拒绝服务。
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

    // 申请名额。拿不到就**立刻拒绝，不排队**——排队的话用户点了发送、
    // 界面一动不动，分不清是卡了还是坏了。
    let permit = st.registry.admit(&session_id).map_err(|r| {
        let status = match r {
            crate::app::registry::Reject::SessionBusy { .. } => StatusCode::CONFLICT,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        ApiError::new(status, r.code(), r.message())
    })?;

    let (chan, rx) = sink::ChannelSink::new();
    let db_path = st.db_path.clone();
    let sid = session_id.clone();

    // ★ 真正干活的在 blocking 线程上。
    //
    //   一次提问是几十秒到几分钟的**同步**代码（reqwest::blocking 打模型
    //   和 ScrapeCreators，rusqlite 写库）。放在 async worker 线程上会把
    //   整个 HTTP 服务占死，而且不报任何错——表现就是"卡住"。
    //
    //   名额（permit）move 进闭包：闭包结束时 Drop，名额归还。panic 也走
    //   Drop，所以"每条退出路径都释放"是类型系统保证的。
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // ★ 这个执行**自己开一条连接**，不用 AppState 里那条只读的。
        //   它要 &mut SqliteStore 并且一占几分钟，占那条的话会话列表就
        //   打不开了。WAL 模式下读不挡写、写不挡读。
        let mut store = match SqliteStore::open(&db_path) {
            Ok(s) => s,
            Err(e) => {
                chan.emit_json(&crate::wire::error_json(&format!("开库失败: {e}")));
                return;
            }
        };
        let cfg = crate::agent::runner::LoopConfig::default();
        let req = crate::app::turn::TurnRequest {
            session_id: &sid,
            question: &question,
            provider,
            config: &cfg,
        };
        // execute 出错时已经往 sink 推过 error 事件了，这里只补一条服务端
        // 日志——浏览器那边不需要再来一遍。
        if let Err(e) = crate::app::turn::execute(&mut store, &req, &chan) {
            eprintln!("会话 {sid} 这一轮失败: {e}");
        }
    });

    // 事件从有界队列流出来，一条一行。
    //
    // keep_alive 每 15 秒发一个 SSE 注释行（`:`）。一次提问里下载视频那段
    // 可能一分钟没有任何事件，中间的反向代理会把空闲连接掐掉。
    // 注释行不是 `data:` 开头，前端本来就跳过它。
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|line| {
        Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(line))
    });
    Ok(axum::response::sse::Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
        )
        .into_response())
}

#[derive(serde::Deserialize)]
struct TurnBody {
    question: String,
    /// 不给就按环境变量里配了谁的 key 自动挑。
    provider: Option<String>,
}

/// 会话相关的「没找到」。消息刻意含糊，见上面 handler 的说明。
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
pub fn run(db_path: &str, addr: &str) -> Result<()> {
    // open 只检查迁移版本，不执行 DDL——版本不对会在这里就报错并告诉人
    // 该跑 `clipknow migrate`，而不是等到第一次查询才炸。
    // ★ 凭证密钥在**开库之前**读。
    //   没配密钥是最常见的启动失败，而它和数据库一点关系都没有——排在后面
    //   的话，一个没迁移的库会先报「版本落后」，把真正的原因盖住。
    let verifier = auth::Verifier::from_env()?;
    let store = SqliteStore::open(db_path)?;
    let state = Arc::new(AppState::new(store, db_path.to_string(), verifier));

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
        let sid = crate::app::sessions::create(&mut store, "u_alice", Some("阿的会话")).unwrap();
        crate::app::sessions::create(&mut store, "u_bob", Some("波的会话")).unwrap();
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
            Arc::new(AppState::new(
                store,
                ":memory:".into(),
                auth::Verifier::new(TEST_SECRET),
            )),
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
    // 这里只测拒绝——它们全都在 spawn_blocking 之前返回，不需要模型。

    fn post_turn(
        srv: &testserver::TestServer,
        user: &str,
        session: &str,
        body: &str,
    ) -> reqwest::blocking::Response {
        reqwest::blocking::Client::new()
            .post(srv.url(&format!("/internal/sessions/{session}/turns")))
            .bearer_auth(token_for(user))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .unwrap()
    }

    /// ★ 拿别人的会话 id 来提问：404，而且**不占用执行名额**。
    ///
    /// 名额的申请必须排在归属校验后面。反过来的话，随便拼一串别人的会话 id
    /// 狂发请求就能把名额占光——一个不用登录成别人就能做到的拒绝服务。
    #[test]
    fn 拿别人的会话提问是_404_且不占名额() {
        let (st, alice_sid) = two_users();
        let reg = Arc::clone(&st.registry);
        let srv = testserver::spawn(st);

        let r = post_turn(&srv, "u_bob", &alice_sid, r#"{"question":"偷看"}"#);
        assert_eq!(r.status(), 404);
        assert_eq!(reg.running_count(), 0, "被拒的请求占住了执行名额");
    }

    #[test]
    fn 对不存在的会话提问是_404() {
        let (st, _) = two_users();
        let srv = testserver::spawn(st);
        assert_eq!(
            post_turn(&srv, "u_alice", "根本没有", r#"{"question":"喂"}"#).status(),
            404
        );
    }

    #[test]
    fn 空问题是_400() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        for body in [r#"{"question":""}"#, r#"{"question":"   "}"#] {
            assert_eq!(
                post_turn(&srv, "u_alice", &sid, body).status(),
                400,
                "{body}"
            );
        }
    }

    /// provider 来自浏览器，是外部输入。必须走白名单。
    #[test]
    fn 不认识的_provider_是_400_但空的当作没给() {
        let (st, sid) = two_users();
        let srv = testserver::spawn(st);
        let bad = post_turn(
            &srv,
            "u_alice",
            &sid,
            r#"{"question":"喂","provider":"--db"}"#,
        );
        assert_eq!(bad.status(), 400);

        // ★ 空串当作「没给」，不能变成「不认识的 provider: 」。
        //   传空串是调用方最容易犯的错（Go 那边就犯过一次），而报错
        //   「不认识的 provider: 」跟真实原因一个字都不沾。
        let empty = post_turn(&srv, "u_alice", &sid, r#"{"question":"喂","provider":""}"#);
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
            .post(srv.url(&format!("/internal/sessions/{sid}/turns")))
            .header("content-type", "application/json")
            .body(r#"{"question":"喂"}"#)
            .send()
            .unwrap();
        assert_eq!(r.status(), 401);
    }

    #[test]
    fn 建会话时归属在创建那一刻就写进去() {
        let (st, _) = two_users();
        let srv = testserver::spawn(st);
        let r = reqwest::blocking::Client::new()
            .post(srv.url("/internal/sessions"))
            .bearer_auth(token_for("u_bob"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .unwrap();
        assert_eq!(r.status(), 201);
        let id = r.json::<serde_json::Value>().unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        // 波能看到它
        let mine: serde_json::Value = get_as(&srv, "u_bob", "/internal/sessions").json().unwrap();
        assert!(
            mine["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == id),
            "刚建的会话不在自己列表里：{mine}"
        );
        // 阿看不到
        let other: serde_json::Value = get_as(&srv, "u_alice", "/internal/sessions")
            .json()
            .unwrap();
        assert!(
            !other["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == id),
            "别人的会话出现在了阿的列表里：{other}"
        );
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
        assert_eq!(v["accepting"], true);

        let _p = reg.admit("s-x").unwrap();
        let v: serde_json::Value = reqwest::blocking::get(srv.url("/internal/health"))
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(v["running_turns"], 1, "health 报的在跑数不对: {v}");
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
}

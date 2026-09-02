//! ClipKnow 命令行入口。
//!
//! 第一版就三个命令：
//!   clipknow ask <url> <问题>   分析一个视频并回答问题
//!   clipknow show <url>         看库里存了这个视频的什么（调试用，会很常用）
//!   clipknow list               列出已抓过的视频
//!
//! `ask` 的完整流程：
//!   1. 解析 URL      → 认出平台和视频 ID       [纯函数]
//!   2. 查库          → 抓过了就跳到第 5 步，省钱
//!   3. 调 SC         → 元数据 + 文字稿 + 评论
//!   4. 写库
//!   5. 拼 prompt     → 标题+作者+简介+文字稿+评论
//!   6. 调大模型      → 一次调用，非流式
//!   7. 打印答案
//!
//! 注意第 6 步：**没有 agent 循环**。单视频问答的证据在开始前就全定了，
//! 一次调用足够，加循环是绕路。等第二步做「找博主」这类需要边找边看的
//! 需求时，循环才会真正需要。

use clap::{Parser, Subcommand};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use clipknow::agent::llm::{LlmClient, ModelRequest, Msg, Provider, StopReason, build_client};
use clipknow::agent::runner::{
    LoopConfig, TurnDeps, TurnOutcome, TurnResult, echo_received, run_turn, run_turn_observed,
};
use clipknow::agent::vision::{VisionClient, build_vision_client};
use clipknow::content::evidence::{
    QUESTION_CLOSE, QUESTION_OPEN, SINGLE_VIDEO_SYSTEM_PROMPT, build_evidence, format_date,
    format_duration, with_signature,
};
use clipknow::content::model::TurnStatus;
use clipknow::error::{ClipKnowError, Result};
use clipknow::ingest::scrapecreators::ScrapeCreators;
use clipknow::ingest::url;
use clipknow::store::sqlite::SqliteStore;
use clipknow::store::{Store, StoredVideo};
use clipknow::wire::{NdjsonSink, done_json, error_json, hello_json, usage_json};

#[derive(Parser)]
#[command(name = "clipknow", about = "分析社媒视频内容", version)]
struct Cli {
    /// 数据库文件路径
    #[arg(long, default_value = "clipknow.db", global = true)]
    db: String,

    /// 用哪家大模型：deepseek 或 anthropic。
    /// 不指定时按环境变量里设了谁的 key 自动挑（两家都有则优先 DeepSeek）。
    #[arg(long, global = true)]
    provider: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 分析一个视频并回答问题
    Ask {
        /// 视频链接（YouTube / TikTok / Instagram）
        url: String,
        /// 你的问题
        question: String,
        /// 忽略缓存，强制重新抓取
        #[arg(long)]
        refresh: bool,
    },
    /// 看看库里存了这个视频的什么
    Show {
        url: String,
        /// 连原始 JSON 一起打印
        #[arg(long)]
        raw: bool,
    },
    /// 发现类需求：找博主、找素材。不给问题就进交互模式。
    Find {
        /// 你的问题。不给就进交互模式，可以连续追问
        question: Option<String>,
        /// 接着最近一次有活动的会话聊
        #[arg(long)]
        continue_: bool,
    },
    /// 列出历史会话
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 跑一次提问，把进度按 NDJSON 打到 stdout。**给 web 服务用的**。
    ///
    /// 人要看的话直接用 `find`。这个模式下 stdout 只有 JSON，
    /// 一行一个事件；日志走 stderr。
    Turn {
        /// 接着哪个会话。不给就新建一个，新会话的 id 在 hello 那一行里。
        #[arg(long)]
        session: Option<String>,
        /// 这一轮的问题
        question: String,

        // ── 闸门覆盖，给 evals 用 ──────────────────────
        //
        // 不给就用默认值。存在的理由：**闸门要能被便宜地触发**。
        //
        // 默认的工具调用上限是 25、压缩阈值 40 万 token（实测一次提问约
        // 2.7 万，得连问十几轮才到）。照默认值测这些闸门，一轮 eval 烧掉的
        // SC 配额比一周用的还多——而跑不起的 eval 等于没有。
        //
        // 压低之后测的是「闸门逻辑对不对」，不是「25 和 40 万这两个数字定得
        // 对不对」。后者靠一次真实冷跑单独验。
        /// 迭代上限
        #[arg(long)]
        max_iterations: Option<usize>,
        /// 第几轮插入收敛提示
        #[arg(long)]
        convergence_iteration: Option<usize>,
        /// 外部端点调用上限
        #[arg(long)]
        max_tool_calls: Option<usize>,
        /// 单次提问最多分析几条视频
        #[arg(long)]
        max_video_analyses: Option<usize>,
        /// 上下文预算（token）
        #[arg(long)]
        context_budget: Option<usize>,
        /// 压缩触发线（token）
        #[arg(long)]
        compaction_threshold: Option<usize>,
        /// 压缩后的目标大小（token）
        #[arg(long)]
        compaction_target: Option<usize>,
    },
    /// 列出已抓过的视频
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn main() {
    // 允许把 key 放在项目目录的 .env 里；没有这个文件也完全正常
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("\n错误: {e}");
        std::process::exit(1);
    }
}

/// 命令行给的闸门覆盖值。全是 Option，None = 用默认。
#[derive(Debug, Default)]
struct GateOverrides {
    max_iterations: Option<usize>,
    convergence_iteration: Option<usize>,
    max_tool_calls: Option<usize>,
    max_video_analyses: Option<usize>,
    context_budget: Option<usize>,
    compaction_threshold: Option<usize>,
    compaction_target: Option<usize>,
}

impl GateOverrides {
    /// 套到默认配置上，并检查改完之后不变量还成立。
    fn apply(&self) -> Result<LoopConfig> {
        let d = LoopConfig::default();
        let max_iter = self.max_iterations.unwrap_or(d.max_iterations);
        // 只压低迭代上限、没管收敛点时，让收敛点**按比例跟着走**（默认值本来
        // 就是上限的 8/10）。不然 `--max-iterations 3` 会因为默认收敛点 16 比它
        // 大而被拒——而压低上限恰恰是 eval 最常用的覆盖。
        let converge = self
            .convergence_iteration
            .unwrap_or_else(|| (max_iter * 8 / 10).max(1));
        let cfg = LoopConfig {
            max_iterations: max_iter,
            convergence_iteration: converge,
            max_tool_calls: self.max_tool_calls.unwrap_or(d.max_tool_calls),
            max_video_analyses: self.max_video_analyses.unwrap_or(d.max_video_analyses),
            context_budget_tokens: self.context_budget.unwrap_or(d.context_budget_tokens),
            compaction_threshold: self.compaction_threshold.unwrap_or(d.compaction_threshold),
            compaction_target_tokens: self.compaction_target.unwrap_or(d.compaction_target_tokens),
        };

        // ★ 覆盖值也要守不变量，不然 eval 会在一个不可能的配置上跑出
        //   看似有意义的结果。这几条在默认配置上有测试钉着，覆盖之后
        //   同样要成立。
        if cfg.max_iterations == 0 {
            return Err(ClipKnowError::BadRequest("max-iterations 不能是 0".into()));
        }
        if cfg.convergence_iteration >= cfg.max_iterations {
            return Err(ClipKnowError::BadRequest(format!(
                "convergence-iteration({}) 必须小于 max-iterations({})——\
                 否则收敛提示永远插不进去",
                cfg.convergence_iteration, cfg.max_iterations
            )));
        }
        if cfg.max_video_analyses >= cfg.max_tool_calls {
            return Err(ClipKnowError::BadRequest(format!(
                "max-video-analyses({}) 必须小于 max-tool-calls({})——\
                 否则视觉闸门永远不会先触发，等于形同虚设",
                cfg.max_video_analyses, cfg.max_tool_calls
            )));
        }
        if cfg.compaction_target_tokens >= cfg.compaction_threshold {
            return Err(ClipKnowError::BadRequest(format!(
                "compaction-target({}) 必须小于 compaction-threshold({})——\
                 否则压完还在阈值以上，等于没压",
                cfg.compaction_target_tokens, cfg.compaction_threshold
            )));
        }
        Ok(cfg)
    }
}

/// `--provider` 的字符串 → 枚举。拼错时明确报错，不悄悄用默认那家。
fn parse_provider(s: Option<&str>) -> Result<Option<Provider>> {
    match s {
        Some(v) => Provider::parse(v).map(Some).ok_or_else(|| {
            ClipKnowError::BadRequest(format!(
                "不认识的 provider: {v}（可选 deepseek / anthropic）"
            ))
        }),
        None => Ok(None),
    }
}

fn run(cli: Cli) -> Result<()> {
    // ★ turn 单独提到最前面，什么都还没做就交给它。
    //
    //   因为它要**先建立 NDJSON 输出通道，再干活**——在那之前的任何失败
    //   都只能喊给 stderr，而 Go 那边只看 stdout，浏览器就是一片空白。
    //   原来「解析 provider」和「打开数据库」在这一步之前，于是数据库被锁死
    //   （另一个进程占着写锁超过 busy_timeout）时，网页上什么提示都没有。
    //
    //   别的命令是给人看的，stderr 就在眼前，不需要这个待遇。
    if let Command::Turn {
        session,
        question,
        max_iterations,
        convergence_iteration,
        max_tool_calls,
        max_video_analyses,
        context_budget,
        compaction_threshold,
        compaction_target,
    } = cli.command
    {
        let overrides = GateOverrides {
            max_iterations,
            convergence_iteration,
            max_tool_calls,
            max_video_analyses,
            context_budget,
            compaction_threshold,
            compaction_target,
        };
        return cmd_turn_json(
            &cli.db,
            cli.provider.as_deref(),
            session,
            &question,
            &overrides,
        );
    }

    // --provider 给了但拼错时，明确报错而不是悄悄用默认的那家
    let provider = parse_provider(cli.provider.as_deref())?;

    let mut store = SqliteStore::open(&cli.db)?;
    match cli.command {
        Command::Ask {
            url,
            question,
            refresh,
        } => cmd_ask(&mut store, &url, &question, refresh, provider),
        Command::Show { url, raw } => cmd_show(&store, &url, raw),
        // 上面提前 return 了。编译器不做跨语句的变体流分析，所以这条必须写出来。
        Command::Turn { .. } => unreachable!("turn 在函数开头就返回了"),
        Command::Find {
            question,
            continue_,
        } => cmd_find(&mut store, question, continue_, provider),
        Command::Sessions { limit } => cmd_sessions(&store, limit),
        Command::List { limit } => cmd_list(&store, limit),
    }
}

/// 确保库里有这个视频；没有（或要求刷新）就去抓。
fn ensure_ingested(store: &mut SqliteStore, raw_url: &str, refresh: bool) -> Result<StoredVideo> {
    let parsed = url::parse(raw_url)?;

    if !refresh && let Some(sv) = store.find_by_native(parsed.platform, &parsed.native_id)? {
        println!("· 命中缓存（加 --refresh 可强制重抓）");
        return Ok(sv);
    }

    println!("· 正在抓取 {} ...", parsed.platform.as_str());
    let sc = ScrapeCreators::from_env()?;
    let fetched = sc.fetch(&parsed, raw_url)?;

    let n_comments = fetched.comments.len();
    let has_transcript = fetched.transcript.is_some();
    store.save(&fetched)?;
    println!(
        "· 抓取完成：文字稿 {} / 评论 {n_comments} 条",
        if has_transcript { "有" } else { "无" }
    );

    store
        .find_by_native(parsed.platform, &parsed.native_id)?
        .ok_or_else(|| ClipKnowError::Fetch {
            platform: parsed.platform.as_str().to_string(),
            message: "刚写进去却读不出来，数据库可能有问题".into(),
        })
}

fn cmd_ask(
    store: &mut SqliteStore,
    raw_url: &str,
    question: &str,
    refresh: bool,
    provider: Option<Provider>,
) -> Result<()> {
    let sv = ensure_ingested(store, raw_url, refresh)?;

    // 材料自带 <video-material> 包裹（内容里伪造的标签已被中和），
    // 用户的问题单独用 <user-question> 包起来——系统提示词里说了，
    // 只有这个标签里的内容才是真正要执行的指令。
    let evidence = build_evidence(&sv);
    let prompt = format!("{evidence}\n{QUESTION_OPEN}\n{question}\n{QUESTION_CLOSE}");

    // 这里拿到的是 Box<dyn LlmClient>，下面的代码完全不知道背后是哪一家
    let llm = build_client(provider)?;
    println!("· 正在问 {} ...\n", llm.model_name());

    let resp = llm.complete(&ModelRequest {
        system: SINGLE_VIDEO_SYSTEM_PROMPT.to_string(),
        messages: vec![Msg::user(prompt)],
        max_tokens: llm.max_tokens_limit(),
        tools: vec![],
    })?;

    println!("{}", with_signature(&resp.text));

    if resp.stop_reason == StopReason::MaxTokens {
        eprintln!("\n(注意：回答因为达到长度上限被截断了)");
    }
    eprintln!(
        "\n— {} · {} 输入 / {} 输出 token，约 ${:.4}",
        llm.model_name(),
        resp.input_tokens,
        resp.output_tokens,
        llm.pricing()
            .cost_usd(resp.input_tokens, 0, resp.output_tokens)
    );
    Ok(())
}

/// `find`：发现类需求。给了问题就跑一次退出，不给就进交互模式。
///
/// 交互模式是因为这一版的核心场景本来就是多轮追问——「第三个那人多久发一条」
/// 之类。每次敲 `--continue` 是别扭的，而且还得记住上次是哪个会话。
fn cmd_find(
    store: &mut SqliteStore,
    question: Option<String>,
    continue_: bool,
    provider: Option<Provider>,
) -> Result<()> {
    let llm = build_client(provider)?;
    let api = ScrapeCreators::from_env()?;
    // 没配 DASHSCOPE_API_KEY 就是 None——不是错误。那时 fetch_video 降级成
    // 只给文字材料并在结果里明写「未配置视觉模型」。
    let vision = build_vision_client();
    let vision_ref = vision.as_deref();
    let cfg = LoopConfig::default();

    let mut session_id = if continue_ {
        match store.latest_session()? {
            Some(s) => {
                let n = store.count_history(&s.id)?;
                println!(
                    "· 接着聊：{}（{n} 条历史）",
                    s.title.as_deref().unwrap_or("(无标题)")
                );
                s.id
            }
            None => {
                println!("· 没有历史会话，开一个新的");
                store.create_session(None)?
            }
        }
    } else {
        store.create_session(None)?
    };

    // 一次性模式
    if let Some(q) = question {
        println!("{}", echo_received(&q));
        return one_turn(store, &*llm, &api, vision_ref, &cfg, &session_id, &q).map(|_| ());
    }

    // 交互模式。
    //
    // 用 rustyline 而不是裸 `read_line`：后者会把终端发来的**一切**当成人打的字。
    // 实跑撞过一次——终端的转义序列应答（背景色查询、光标位置报告）进了 stdin，
    // 一整轮被当成提问发给了模型；同一次会话里还丢了四个字，导致模型接着上一轮
    // 的话题答错了方向，白花 25 次工具调用。
    // rustyline 在 raw 模式下自己解析转义序列，不认识的直接丢掉。
    // 顺带白得方向键调历史、Ctrl-C 只清当前行而不是杀进程。
    println!(
        "· {} · 画面 {} · 输入问题回车；↑ 调历史；/new 开新会话，/quit 退出",
        llm.model_name(),
        vision_ref.map_or("未启用", |v| v.model_name())
    );
    let mut rl =
        DefaultEditor::new().map_err(|e| ClipKnowError::Llm(format!("初始化输入失败: {e}")))?;
    loop {
        let line = match rl.readline("\n> ") {
            Ok(l) => l,
            // Ctrl-C：只放弃当前这行，不退出
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(e) => return Err(ClipKnowError::Llm(format!("读输入失败: {e}"))),
        };
        let q = line.trim();
        match q {
            "" => continue,
            "/quit" | "/exit" => return Ok(()),
            "/new" => {
                session_id = store.create_session(None)?;
                println!("· 已开新会话（上一个已存好，`--continue` 能回去）");
                continue;
            }
            _ => {}
        }
        let _ = rl.add_history_entry(q);
        // ★ 回显收到的问题。终端可能吞字（见 echo_received 的注释），
        //   在发请求、花钱之前先让你看见程序实际拿到了什么。
        println!("{}", echo_received(q));
        // 单次提问失败不该把你踢出交互
        if let Err(e) = one_turn(store, &*llm, &api, vision_ref, &cfg, &session_id, q) {
            eprintln!("错误: {e}");
        }
    }
}

/// 跑一次提问：读历史 → 循环 → 落库 → 打印。
fn one_turn(
    store: &mut SqliteStore,
    llm: &dyn LlmClient,
    api: &ScrapeCreators,
    vision: Option<&dyn VisionClient>,
    cfg: &LoopConfig,
    session_id: &str,
    question: &str,
) -> Result<()> {
    // 按 turn 分组传进去：压缩要按 turn 边界切，扁平的条目列表没有这个信息。
    // 这个查询已经过滤了失败的 turn，也已跳过被摘要覆盖的部分。
    let history = store.load_turns_with_items(session_id)?;
    let is_first_turn = history.is_empty();
    let res = run_turn(llm, api, store, vision, &history, question, cfg);

    persist_turn(
        store,
        llm.model_name(),
        session_id,
        question,
        is_first_turn,
        &res,
    )?;

    match res.outcome {
        TurnOutcome::Done => println!("\n{}", with_signature(&res.answer)),
        TurnOutcome::IterationCap => println!(
            "\n(跑了 {} 轮还没收敛，已停下。已经查到的都在库里，可以换个更具体的问法)",
            res.iterations
        ),
        TurnOutcome::ContextBudget { used, limit } => {
            println!(
                "\n⚠ 这个会话的历史太长了（约 {used} token，上限 {limit}）。\n\
                 \x20 继续问会被模型拒掉。输入 /new 开新会话（当前会话已存好，\
                 随时 `--continue` 回来），或用 `clipknow ask <链接> \"...\"` 问单个视频。"
            );
            return Ok(());
        }
        TurnOutcome::Truncated => println!(
            "\n{}\n\n⚠ 回答达到长度上限被截断了，上面这段是残缺的。\n\
             \x20 这个 turn 已标记为失败，`--continue` 不会带上它。换个更聚焦的问法再试。",
            res.answer
        ),
        TurnOutcome::ProtocolError(ref e) => println!(
            "\n模型返回了没见过的结束原因: {e}\n（已拿到的内容：{}）",
            if res.answer.is_empty() {
                "无"
            } else {
                &res.answer
            }
        ),
        TurnOutcome::ModelError(ref e) => println!("\n模型调用失败: {e}"),
    }

    // 快满时提前提醒，别等撞墙。
    // 用 run_turn 报回的**真实** prompt_tokens——原来这里自己 load_history
    // 一遍、用一套过时的估算公式（chars*10/19，runner 早就改成按字符类型算了）。
    let used = res.context_tokens;
    if used * 10 > cfg.context_budget_tokens * 9 {
        eprintln!(
            "\n(提示：会话历史已用约 {used} / {} token，接近上限，建议 /new 开新会话)",
            cfg.context_budget_tokens
        );
    }

    // 视觉分析的账单在千问那边（人民币计价），和主模型的美元账分开写——
    // 混在一个数字里既不准也看不出钱花在哪。
    let vis_note = if res.video_analyses > 0 {
        format!(
            " · 画面 {} 条 / {} token（约 ¥{:.3}）",
            res.video_analyses,
            res.video_tokens,
            f64::from(res.video_tokens) / 1_000_000.0 * 3.0
        )
    } else {
        String::new()
    };
    eprintln!(
        "\n— {} 轮 · {} 次工具调用（扣 {} credit）{vis_note} · 输入 {}（其中 {} 命中缓存）/ 输出 {} token · 约 ${:.4}",
        res.iterations,
        res.tool_calls_made,
        res.credits_charged,
        res.input_tokens,
        res.cached_input_tokens,
        res.output_tokens,
        llm.pricing()
            .cost_usd(res.input_tokens, res.cached_input_tokens, res.output_tokens)
    );
    Ok(())
}

/// 把一次 turn 的结果落库。
///
/// CLI（`one_turn`）和 web 子进程（`cmd_turn_json`）共用**同一份**。两处各写
/// 一遍必然漂移，而这里的规则都是有不变量的：
///   - 失败的 turn 也要落库（`load_history` 那边会跳过它，但历史本身要完整）
///   - 摘要必须在 `save_turn` **之后**写：它挂在最新那个 turn 上，
///     而那个 turn 是 save_turn 刚建出来的
///   - 上下文闸门那一轮**什么都不落**：请求根本没发出去，没有任何事发生
fn persist_turn(
    store: &mut SqliteStore,
    model: &str,
    session_id: &str,
    question: &str,
    is_first_turn: bool,
    res: &TurnResult,
) -> Result<()> {
    let status = match &res.outcome {
        TurnOutcome::Done => TurnStatus::Done,
        TurnOutcome::IterationCap => TurnStatus::Failed("超过迭代上限".into()),
        // 残缺的答案不能标成成功：下次 --continue 时历史里会带着半句话
        TurnOutcome::Truncated => TurnStatus::Failed("回答被长度上限截断".into()),
        TurnOutcome::ProtocolError(e) => TurnStatus::Failed(format!("协议异常: {e}")),
        TurnOutcome::ContextBudget { .. } => TurnStatus::Failed("上下文预算不足".into()),
        TurnOutcome::ModelError(e) => TurnStatus::Failed(format!("模型调用失败: {e}")),
    };

    if matches!(res.outcome, TurnOutcome::ContextBudget { .. }) {
        return Ok(());
    }
    store.save_turn(session_id, model, status, &res.items)?;
    if let Some((text, upto)) = &res.pending_summary {
        store.save_compaction(session_id, text, *upto)?;
    }
    // 第一次提问顺手拿它当标题，会话列表才认得出是哪次
    if is_first_turn {
        store.set_session_title(session_id, &truncate_chars(question, 40))?;
    }
    Ok(())
}

/// 每种结局该对用户说什么。
///
/// 放在 Rust 这边而不是让 Go 或前端各维护一份映射——那样加一个 outcome
/// 就要改三处，而漏改的表现是界面上一片空白。
fn outcome_note(res: &TurnResult, cfg: &LoopConfig) -> String {
    // Done 但历史快满了：提前提醒，别等撞墙。CLI 那边也打这句。
    if matches!(res.outcome, TurnOutcome::Done) {
        return if res.context_tokens * 10 > cfg.context_budget_tokens * 9 {
            format!(
                "会话历史已用约 {} / {} token，接近上限，建议开新会话。",
                res.context_tokens, cfg.context_budget_tokens
            )
        } else {
            String::new()
        };
    }
    match &res.outcome {
        TurnOutcome::Done => unreachable!("上面已经返回了"),
        TurnOutcome::IterationCap => format!(
            "跑了 {} 轮还没收敛，已停下。已经查到的都在库里，可以换个更具体的问法。",
            res.iterations
        ),
        TurnOutcome::Truncated => "回答达到长度上限被截断了，上面这段是残缺的。\
             这一轮已标记为失败，下次不会带上它。换个更聚焦的问法再试。"
            .into(),
        TurnOutcome::ProtocolError(e) => format!("模型返回了没见过的结束原因：{e}"),
        TurnOutcome::ContextBudget { used, limit } => format!(
            "这个会话的历史太长了（约 {used} / {limit} token）。继续问会被模型拒掉，\
             请开一个新会话——当前会话已存好，随时能回去。"
        ),
        // 不加「模型调用失败：」前缀——e 本身就是 ClipKnowError::Llm，
        // 渲染出来已经带「大模型调用失败: 」了，加了就是重一遍。
        TurnOutcome::ModelError(e) => model_error_note(e),
    }
}

/// 把供应商的原始报错翻成能行动的一句话。
///
/// 原样吐给用户是没用的——`HTTP 400 Bad Request: Content Exists Risk` 这种
/// 话，看到的人既不知道发生了什么，也不知道下一步该干什么。
///
/// 这里只认**确实见过、而且有明确对策**的几种，其余原样保留：编一套看似
/// 全面的映射，撞上没覆盖的错误时反而会给出误导性的建议。
fn model_error_note(e: &str) -> String {
    // DeepSeek 的内容审查。它审的是**整个请求**（系统提示词 + 全部历史 +
    // 这一轮抓到的材料），所以触发点常常在抓回来的搜索结果里，而不是用户
    // 的问题上。
    //
    // 这一轮已经标成 failed，而 load_turns_with_items 只带 done 的 turn，
    // 所以触发审查的那批材料不会污染后续提问——这一点要明说，不然用户会
    // 以为整个会话废了。
    if e.contains("Content Exists Risk") {
        // ⚠️ 别在这里建议「换 Claude」——Anthropic 的工具调用这一版还没实现
        // （agent/llm.rs 会直接拒掉带 tools 的请求），而这条路径必然带工具。
        // 给一个必然失败的建议比不给建议更糟。
        return "DeepSeek 的内容审查拒绝了这次请求。它审查的是**整个请求**，\
                包括这一轮抓回来的材料——触发点多半在搜索结果里，而不是你的问题。\
                \n这一轮已标记为失败，不会带进后续的历史，接着问别的没问题。\
                \n原样重问大概率还是同样的结果（同样的搜索会拿回同样的材料）。\
                换个更窄的问法能绕开：直接给视频链接让它只看那一条，\
                而不是让它去搜——搜索会把话题相关的一大堆东西都捞回来。"
            .into();
    }
    if e.contains("rate limit") || e.contains("429") {
        return format!("{e}\n（限流，等一会儿再试就行）");
    }
    e.to_string()
}

/// `clipknow turn` —— 跑一次提问，进度按 NDJSON 打到 stdout。
///
/// **stdout 只有 JSON。** 出错也走 JSON（`{"t":"error"}`），不然 Go 那边
/// 只能看到一个非零退出码，没法在界面上说清出了什么事。同时照样返回 Err，
/// 让手敲这条命令时有个非零退出码可看。
///
/// ★ 它自己开库、自己解析 provider，而不是让 `run` 先做好递进来——
///   因为**第一件事必须是建立输出通道**。在那之前失败的话，错误只能进
///   stderr，而 Go 只读 stdout，浏览器上就是一片空白（实测：HTTP 200，
///   响应体 0 字节，页面把进度收起来，什么都不说）。
fn cmd_turn_json(
    db_path: &str,
    provider: Option<&str>,
    session: Option<String>,
    question: &str,
    overrides: &GateOverrides,
) -> Result<()> {
    // 嘴要先长出来。下面每一步失败都能被它报出去。
    let sink = NdjsonSink::new(std::io::stdout());

    // 宏：出错时先把错误推出去，再往上抛
    macro_rules! bail {
        ($e:expr) => {{
            let e = $e;
            sink.emit(&error_json(&e.to_string()));
            return Err(e);
        }};
    }

    // ★ 纯参数校验排在最前面：配置写错不该等到建完模型客户端、连完 SC
    //   之后才报。这两步要读环境变量、可能失败，而它们的错误会盖住真正的
    //   原因（「缺少 SCRAPECREATORS_API_KEY」而不是「参数写错了」）。
    let cfg = match overrides.apply() {
        Ok(c) => c,
        Err(e) => bail!(e),
    };
    let provider = match parse_provider(provider) {
        Ok(p) => p,
        Err(e) => bail!(e),
    };
    // ★ 开库放在 sink 之后。数据库被锁死（另一个进程占着写锁超过
    //   busy_timeout）时，这条错误现在能到浏览器上。
    let mut store_owned = match SqliteStore::open(db_path) {
        Ok(s) => s,
        Err(e) => bail!(e),
    };
    let store = &mut store_owned;

    let llm = match build_client(provider) {
        Ok(l) => l,
        Err(e) => bail!(e),
    };
    let api = match ScrapeCreators::from_env() {
        Ok(a) => a,
        Err(e) => bail!(e),
    };
    let vision = build_vision_client();
    let vision_ref = vision.as_deref();

    // 会话由 Rust 这边建，不由 Go 建——这样**所有写操作都在这个进程里**，
    // Go 只读。新会话的 id 在 hello 那一行报出去，Go 记下来下次传回。
    let session_id = match session {
        Some(id) => {
            if !store.session_exists(&id)? {
                bail!(ClipKnowError::BadRequest(format!(
                    "没有这个会话: {id}（会话由 `turn` 自己创建，id 在 hello 那一行里）"
                )))
            }
            id
        }
        None => match store.create_session(None) {
            Ok(id) => id,
            Err(e) => bail!(e),
        },
    };

    sink.emit(&hello_json(
        &session_id,
        llm.model_name(),
        vision_ref.map(|v| v.model_name()),
    ));

    let history = match store.load_turns_with_items(&session_id) {
        Ok(h) => h,
        Err(e) => bail!(e),
    };
    let is_first_turn = history.is_empty();

    let res = run_turn_observed(
        TurnDeps {
            llm: &*llm,
            api: &api,
            store,
            vision: vision_ref,
        },
        &history,
        question,
        &cfg,
        &sink,
    );

    // ★ 落库在推 usage/done **之前**：Go 收到 done 就会去刷会话历史，
    //   那时候这一轮必须已经在库里了。
    if let Err(e) = persist_turn(
        store,
        llm.model_name(),
        &session_id,
        question,
        is_first_turn,
        &res,
    ) {
        sink.emit(&error_json(&format!("写库失败: {e}")));
        return Err(e);
    }

    let cost = llm
        .pricing()
        .cost_usd(res.input_tokens, res.cached_input_tokens, res.output_tokens);
    sink.emit(&usage_json(&res, cost));
    sink.emit(&done_json(&res.outcome, &outcome_note(&res, &cfg)));
    Ok(())
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn cmd_sessions(store: &SqliteStore, limit: usize) -> Result<()> {
    let ss = store.list_sessions(limit)?;
    if ss.is_empty() {
        println!("还没有会话。跑一次 `clipknow find` 开始。");
        return Ok(());
    }
    for s in &ss {
        let turns = store.list_turns(&s.id)?;
        let failed = turns
            .iter()
            .filter(|t| !matches!(t.status, TurnStatus::Done))
            .count();
        println!(
            "{}  {:<40} {} 次提问{}",
            format_date(s.created_at),
            truncate_chars(s.title.as_deref().unwrap_or("(无标题)"), 40),
            turns.len(),
            if failed > 0 {
                format!("（{failed} 次失败）")
            } else {
                String::new()
            }
        );
    }
    println!(
        "\n共 {} 个会话。`clipknow find --continue` 接着最近一个聊。",
        ss.len()
    );
    Ok(())
}

fn cmd_show(store: &SqliteStore, raw_url: &str, raw: bool) -> Result<()> {
    let parsed = url::parse(raw_url)?;
    let Some(sv) = store.find_by_native(parsed.platform, &parsed.native_id)? else {
        println!("库里没有这个视频。先跑一次 `clipknow ask <url> <问题>` 把它抓进来。");
        return Ok(());
    };

    println!("{}", build_evidence(&sv));

    // 三个端点的抓取产物。以前这里只有详情那一份原始响应，
    // 现在文字稿和评论的也在，「原始数据都在」这句话才算数。
    let artifacts = store.get_artifacts(&sv.video.id)?;

    if artifacts.is_empty() {
        println!("(这条记录是加 artifacts 表之前抓的，没有存原始响应。`--refresh` 重抓可以补上)");
        return Ok(());
    }

    if raw {
        for a in &artifacts {
            println!(
                "=== 原始响应：{} [{}] ===",
                a.kind.label(),
                a.status.as_str()
            );
            match (&a.raw_json, &a.error) {
                (Some(j), _) => println!("{j}\n"),
                (None, Some(e)) => println!("(抓取失败：{e})\n"),
                (None, None) => println!("(没有内容)\n"),
            }
        }
    } else {
        let summary: Vec<String> = artifacts
            .iter()
            .map(|a| format!("{}={}", a.kind.label(), a.status.as_str()))
            .collect();
        println!("抓取状态: {}", summary.join("  "));
        println!("(加 --raw 可以看三个端点的原始响应)");
    }
    Ok(())
}

fn cmd_list(store: &SqliteStore, limit: usize) -> Result<()> {
    let videos = store.list_videos(limit)?;
    if videos.is_empty() {
        println!("库里还没有视频。");
        return Ok(());
    }
    for v in &videos {
        println!(
            "{:<10} {:<8} {}",
            v.platform.as_str(),
            v.duration_sec
                .map(format_duration)
                .unwrap_or_else(|| "-".into()),
            v.title.as_deref().unwrap_or("(无标题)")
        );
        println!("           {}  抓取于 {}", v.url, format_date(v.fetched_at));
    }
    println!("\n共 {} 条", videos.len());
    Ok(())
}

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
use clipknow::agent::runner::{LoopConfig, TurnOutcome, echo_received, run_turn};
use clipknow::content::evidence::{
    QUESTION_CLOSE, QUESTION_OPEN, SINGLE_VIDEO_SYSTEM_PROMPT, build_evidence, format_date,
    format_duration,
};
use clipknow::content::model::TurnStatus;
use clipknow::error::{ClipKnowError, Result};
use clipknow::ingest::scrapecreators::ScrapeCreators;
use clipknow::ingest::url;
use clipknow::store::sqlite::SqliteStore;
use clipknow::store::{Store, StoredVideo};

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

fn run(cli: Cli) -> Result<()> {
    // --provider 给了但拼错时，明确报错而不是悄悄用默认的那家
    let provider = match &cli.provider {
        Some(s) => Some(Provider::parse(s).ok_or_else(|| {
            ClipKnowError::Llm(format!(
                "不认识的 provider: {s}（可选 deepseek / anthropic）"
            ))
        })?),
        None => None,
    };

    let mut store = SqliteStore::open(&cli.db)?;
    match cli.command {
        Command::Ask {
            url,
            question,
            refresh,
        } => cmd_ask(&mut store, &url, &question, refresh, provider),
        Command::Show { url, raw } => cmd_show(&store, &url, raw),
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

    println!("{}", resp.text);

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
        return one_turn(store, &*llm, &api, &cfg, &session_id, &q).map(|_| ());
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
        "· {} · 输入问题回车；↑ 调历史；/new 开新会话，/quit 退出",
        llm.model_name()
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
        if let Err(e) = one_turn(store, &*llm, &api, &cfg, &session_id, q) {
            eprintln!("错误: {e}");
        }
    }
}

/// 跑一次提问：读历史 → 循环 → 落库 → 打印。
fn one_turn(
    store: &mut SqliteStore,
    llm: &dyn LlmClient,
    api: &ScrapeCreators,
    cfg: &LoopConfig,
    session_id: &str,
    question: &str,
) -> Result<()> {
    // 按 turn 分组传进去：压缩要按 turn 边界切，扁平的条目列表没有这个信息。
    // 这个查询已经过滤了失败的 turn，也已跳过被摘要覆盖的部分。
    let history = store.load_turns_with_items(session_id)?;
    let is_first_turn = history.is_empty();
    let res = run_turn(llm, api, store, &history, question, cfg);

    let status = match &res.outcome {
        TurnOutcome::Done => TurnStatus::Done,
        TurnOutcome::IterationCap => TurnStatus::Failed("超过迭代上限".into()),
        // 残缺的答案不能标成成功：下次 --continue 时历史里会带着半句话
        TurnOutcome::Truncated => TurnStatus::Failed("回答被长度上限截断".into()),
        TurnOutcome::ProtocolError(e) => TurnStatus::Failed(format!("协议异常: {e}")),
        TurnOutcome::ContextBudget { .. } => TurnStatus::Failed("上下文预算不足".into()),
        TurnOutcome::ModelError(e) => TurnStatus::Failed(format!("模型调用失败: {e}")),
    };

    // 上下文闸门是在发请求之前拦下的，这一轮什么都没发生，不必落库
    if !matches!(res.outcome, TurnOutcome::ContextBudget { .. }) {
        store.save_turn(session_id, llm.model_name(), status, &res.items)?;
        // 压缩也在终态落库。摘要必须在 save_turn 之后写——它挂在最新那个
        // turn 上，而那个 turn 是 save_turn 刚建的。
        if let Some((text, upto)) = &res.pending_summary {
            store.save_compaction(session_id, text, *upto)?;
        }
        // 第一次提问顺手拿它当标题，`clipknow sessions` 才认得出是哪次
        if is_first_turn {
            store.set_session_title(session_id, &truncate_chars(question, 40))?;
        }
    }

    match res.outcome {
        TurnOutcome::Done => println!("\n{}", res.answer),
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

    eprintln!(
        "\n— {} 轮 · {} 次工具调用（扣 {} credit）· 输入 {}（其中 {} 命中缓存）/ 输出 {} token · 约 ${:.4}",
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

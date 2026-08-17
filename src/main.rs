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

use clipknow::agent::llm::{ModelRequest, Msg, Provider, StopReason, build_client};
use clipknow::content::evidence::{
    QUESTION_CLOSE, QUESTION_OPEN, SYSTEM_PROMPT, build_evidence, format_date, format_duration,
};
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
        system: SYSTEM_PROMPT.to_string(),
        messages: vec![Msg::user(prompt)],
        max_tokens: llm.max_tokens_limit(),
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
            .cost_usd(resp.input_tokens, resp.output_tokens)
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
    if raw {
        println!("=== 原始 JSON ===\n{}", sv.video.raw_json);
    } else {
        println!("(加 --raw 可以看 SC 返回的原始 JSON)");
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

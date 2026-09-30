//! 跑一次提问：**网页和命令行唯一的执行入口**。
//!
//! 这些代码原来在 `main.rs` 的 `cmd_turn_json` 里。搬出来是因为设计文档第
//! 12 节的第 4 条不变量——「只有一个执行入口定义准入规则，网页和 CLI 不各写
//! 一套」。两处各写一遍的话，迟早出现「网页拦得住、命令行拦不住」。
//!
//! 现在两条路是这样的：
//!
//! ```text
//! clipknow turn（命令行 / evals）→ execute() → NdjsonSink  → stdout
//! POST /internal/.../turns（网页）→ execute() → ChannelSink → 有界队列 → SSE
//! ```
//!
//! 中间那一格是同一份代码，两边发出去的 JSON 也完全一样。
//!
//! ## 谁负责什么
//!
//! 这个模块**不管准入**（那是 [`super::registry`]），也**不管归属校验**
//! （那是 [`super::sessions`]）。它假设调用方已经拿到名额、已经确认会话是
//! 这个人的，然后老老实实跑一轮：建模型客户端 → 读历史 → 跑循环 → 落库 →
//! 报用量和结局。

use crate::agent::llm::{Provider, build_client};
use crate::agent::runner::{LoopConfig, TurnDeps, TurnOutcome, TurnResult, run_turn_observed};
use crate::agent::vision::build_vision_client;
use crate::content::model::TurnStatus;
use crate::error::{ClipKnowError, Result};
use crate::ingest::scrapecreators::ScrapeCreators;
use crate::store::sqlite::SqliteStore;
use crate::wire::{TurnSink, done_json, error_json, hello_json, usage_json};

/// 跑一轮要的全部输入。
pub struct TurnRequest<'a> {
    /// 接着哪个会话。**必须已经存在**——建会话是调用方的事
    /// （[`super::sessions::create`]），因为归属必须在创建那一刻就写进去。
    pub session_id: &'a str,
    pub question: &'a str,
    /// `None` = 按环境变量里配了谁的 key 自动挑。
    pub provider: Option<Provider>,
    pub config: &'a LoopConfig,
}

/// 跑一次提问，所有事件通过 `sink` 发出去。
///
/// 出错时**先把错误推给 sink，再往上抛**——上层（HTTP handler 或命令行）
/// 只拿到一个 Err，而用户那边已经看到原因了。顺序反过来的话，进程/请求
/// 结束时浏览器上是一片空白。
pub fn execute(store: &mut SqliteStore, req: &TurnRequest<'_>, sink: &dyn TurnSink) -> Result<()> {
    // 出错时先喊一声再抛
    macro_rules! bail {
        ($e:expr) => {{
            let e = $e;
            sink.emit_json(&error_json(&e.to_string()));
            return Err(e);
        }};
    }

    // 这两步要读环境变量、可能失败。放在读历史之前，因为「没配 key」
    // 应该在花任何时间之前就报出来。
    let llm = match build_client(req.provider) {
        Ok(l) => l,
        Err(e) => bail!(e),
    };
    let api = match ScrapeCreators::from_env() {
        Ok(a) => a,
        Err(e) => bail!(e),
    };
    let vision = build_vision_client();
    let vision_ref = vision.as_deref();

    sink.emit_json(&hello_json(
        req.session_id,
        llm.model_name(),
        vision_ref.map(|v| v.model_name()),
    ));

    // ★ 历史在 begin_turn **之前**读。反过来也不会出错（读历史只认
    //   status='done'，running 的这一轮本来就进不去），但先读后写让「这一轮
    //   看不到它自己」这件事不依赖那个过滤条件。
    let history = match store.load_turns_with_items(req.session_id) {
        Ok(h) => h,
        Err(e) => bail!(e),
    };

    // ★ 提问一被接受就先写库：一行 running 的 turn + 用户的问题 + 标题。
    //
    //   以前是整轮答完才写。实测：跑到一半刷新页面，连自己问的问题都看不到，
    //   会话连标题都没有——看起来像「问题全丢了」，其实只是还没写。
    let (turn_id, _seq) = match store.begin_turn(
        req.session_id,
        llm.model_name(),
        req.question,
        &truncate_chars(req.question, 40),
    ) {
        Ok(t) => t,
        Err(e) => bail!(e),
    };

    let res = run_turn_observed(
        TurnDeps {
            llm: &*llm,
            api: &api,
            store,
            vision: vision_ref,
        },
        &history,
        req.question,
        req.config,
        sink,
    );

    // ★ 收尾落库在推 usage/done **之前**：收到 done 的那一方（Go / 前端 /
    //   evals）会立刻去刷会话历史，那时候这一轮必须已经是终态了。
    //
    //   和 persist_turn 的一处不同：上下文闸门那一轮（ContextBudget）
    //   persist_turn 什么都不落，这里**标成失败**。因为问题已经在 begin_turn
    //   里写进去了，不收尾的话它会永远显示「正在回答」。
    let finished = store
        .finish_turn(&turn_id, req.session_id, final_status(&res), &res.items)
        .and_then(|()| match &res.pending_summary {
            Some((text, upto)) => store.save_compaction(req.session_id, text, *upto),
            None => Ok(()),
        });
    if let Err(e) = finished {
        sink.emit_json(&error_json(&format!("写库失败: {e}")));
        return Err(e);
    }

    let cost = llm
        .pricing()
        .cost_usd(res.input_tokens, res.cached_input_tokens, res.output_tokens);
    sink.emit_json(&usage_json(&res, cost));
    sink.emit_json(&done_json(&res.outcome, &outcome_note(&res, req.config)));
    Ok(())
}

/// `--provider` 的字符串 → 枚举。拼错时明确报错，不悄悄用默认那家。
pub fn parse_provider(s: Option<&str>) -> Result<Option<Provider>> {
    // ★ 空串当作「没给」。
    //
    //   调用方（浏览器的下拉框、Go 的转发、命令行的 --provider）任何一处
    //   传个空串进来，都不该变成「不认识的 provider: 」这种驴唇不对马嘴的
    //   报错。代码不能靠调用方永远写对——这条在 crate::env_var 那里已经
    //   吃过一次亏了（docker-compose 的 ${VAR:-} 传空串，模型名变成空的，
    //   而报错跟模型名一个字都不沾）。
    match s.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => Provider::parse(v).map(Some).ok_or_else(|| {
            ClipKnowError::BadRequest(format!(
                "不认识的 provider: {v}（可选 deepseek / anthropic）"
            ))
        }),
        None => Ok(None),
    }
}

/// 一轮的结局 → 库里的终态。两条落库路径（persist_turn 和 execute）共用。
///
/// 除了 Done 以外全算失败：残缺的答案不能标成成功，不然下一次提问的
/// 历史里会带着半句话。
fn final_status(res: &TurnResult) -> TurnStatus {
    match &res.outcome {
        TurnOutcome::Done => TurnStatus::Done,
        TurnOutcome::IterationCap => TurnStatus::Failed("超过迭代上限".into()),
        TurnOutcome::Truncated => TurnStatus::Failed("回答被长度上限截断".into()),
        TurnOutcome::ProtocolError(e) => TurnStatus::Failed(format!("协议异常: {e}")),
        TurnOutcome::ContextBudget { .. } => TurnStatus::Failed("上下文预算不足".into()),
        TurnOutcome::ModelError(e) => TurnStatus::Failed(format!("模型调用失败: {e}")),
    }
}

/// 把一次 turn 的结果落库。
///
/// **命令行交互模式（`clipknow find` / `ask`）用。** 网页和 `clipknow turn`
/// 走的是 [`execute`] 里的两段式落库（begin_turn / finish_turn）。
///
/// 两处各写
/// 一遍必然漂移，而这里的规则都是有不变量的：
///   - 失败的 turn 也要落库（`load_history` 那边会跳过它，但历史本身要完整）
///   - 摘要必须在 `save_turn` **之后**写：它挂在最新那个 turn 上，
///     而那个 turn 是 save_turn 刚建出来的
///   - 上下文闸门那一轮**什么都不落**：请求根本没发出去，没有任何事发生
pub fn persist_turn(
    store: &mut SqliteStore,
    model: &str,
    session_id: &str,
    question: &str,
    is_first_turn: bool,
    res: &TurnResult,
) -> Result<()> {
    if matches!(res.outcome, TurnOutcome::ContextBudget { .. }) {
        return Ok(());
    }
    store.save_turn(session_id, model, final_status(res), &res.items)?;
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
pub fn outcome_note(res: &TurnResult, cfg: &LoopConfig) -> String {
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
pub fn model_error_note(e: &str) -> String {
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

pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

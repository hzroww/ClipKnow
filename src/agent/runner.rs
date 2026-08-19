//! Agent 循环。
//!
//! 这一版的核心，也是整个项目里唯一必须有循环的地方：要查哪几个博主，
//! 取决于上一步搜到了什么，没法提前写死。
//!
//! 循环是一台状态机（Init / CallingModel / ExecutingTools / Done / Failed）。
//! 把状态显式画出来，是为了让**哪些转移不存在**也变得明确——
//! 大部分 bug 出在不该走通的边被无意走通了。见 7.3 节的三条。

use crate::agent::llm::{LlmClient, ModelRequest, Msg, ToolResult};
use crate::agent::tools::{execute, tool_defs};
use crate::content::evidence::SYSTEM_PROMPT;
use crate::content::model::Item;
use crate::ingest::discovery::DiscoveryApi;
use crate::store::sqlite::SqliteStore;

/// 三道闸门的取值。
///
/// ⚠️ **2026-08-19 重新校准过。** 之前这几个值是基于「DeepSeek 上下文 64K」
/// 算出来的，而那个数字是凭记忆写的、错的——实测 `deepseek-v4-flash` 的窗口是
/// **1M token**，输出上限 384K。原来的 40K 预算把一次正常提问就顶到了 96%，
/// 那是一堵自己造的假墙。
///
/// 现在三道闸门防的东西不一样了：
///   - 上下文预算：从「主要约束」降级成防失控的兜底
///   - 迭代 / 调用上限：仍然是**成本**闸门（窗口大不代表塞满免费）
pub struct LoopConfigDoc;
#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub max_iterations: usize,
    /// 到这一轮时往历史里插一句「该收敛了」。硬砍是「到点就死」，
    /// 提前给模型预算感知，它通常自己就收尾了。
    pub convergence_iteration: usize,
    /// 单次提问的 SC 调用上限。SC 按次计费，这是成本闸门。
    pub max_tool_calls: usize,
    /// 上下文预算。窗口 64K，减去回答的 8192、系统提示词和工具定义，
    /// 再留出估算误差的余量——估少了浪费一点额度，估多了直接失败，代价不对称。
    pub context_budget_tokens: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        LoopConfig {
            // 放宽：原来的 10 轮是怕�based 撑爆上下文，那个顾虑没了。
            // 现在它纯粹是防死循环 + 控成本，20 轮足够复杂任务收敛。
            max_iterations: 20,
            convergence_iteration: 16, // 20 的 8/10
            // SC 按次计费，这个是真金白银，只小幅放宽
            max_tool_calls: 25,
            // 窗口实测 1M（deepseek-v4-flash）。留出：
            //   输出上限 32K + 系统提示词与工具定义约 1K + 一点余量
            // 循环内的判断用 provider 报的**真实** prompt_tokens，不是字符估算，
            // 所以余量不用留得很厚。
            //
            // 这个闸门现在基本不会触发——实测一次「找博主」约 7 万 token，
            // 三次连续追问累计约 13 万。它防的是失控（模型陷入循环疯狂
            // fetch 长文字稿），不是正常用量。
            context_budget_tokens: 940_000,
        }
    }
}

#[derive(Debug)]
pub enum TurnOutcome {
    Done,
    /// 撞了迭代硬上限。超限那一轮的请求没有发出去。
    IterationCap,
    /// 已有历史就超预算了，第一次请求都没发。
    ContextBudget {
        used: usize,
        limit: usize,
    },
    ModelError(String),
}

pub struct TurnResult {
    pub outcome: TurnOutcome,
    pub answer: String,
    /// 这一次提问产生的全部条目，idx 从 1 开始（idx 是 turn 内的编号）。
    pub items: Vec<Item>,
    pub iterations: usize,
    pub tool_calls_made: usize,
    /// SC 实际扣的 credit 合计。不等于 `tool_calls_made`——失败的不扣费。
    pub credits_charged: i64,
    pub input_tokens: u32,
    /// `input_tokens` 里命中前缀缓存的部分。DeepSeek 自动缓存，
    /// 循环从第二轮起绝大部分历史都会命中——实测 4094 里命中 3968。
    pub cached_input_tokens: u32,
    pub output_tokens: u32,
}

/// 估算 token 数。
///
/// **按字符类型分开算**，因为差别很大（2026-08-19 拿 deepseek-v4-flash 实测）：
///
/// | 内容 | 字符/token |
/// |---|---|
/// | 纯中文口播 | 1.85 |
/// | 中英混排（渲染后的搜索结果） | 1.93 |
/// | JSON / 数字 | 2.96 |
/// | 纯英文散文 | 6.12 |
///
/// 旧版按 1.9 一刀切，对纯英文高估 222%——会让英文多的会话在真实容量的
/// 三分之一处就被拦下。
///
/// ASCII 那一档的跨度很大（实测 1.68 ～ 6.12），**没有单一比值能既准确又安全**：
///
/// | ASCII 内容 | 字符/token |
/// |---|---|
/// | 句柄 + 数字（`(@laogao) \| 播放 4780866`） | 1.68 |
/// | URL | 2.45 |
/// | JSON | 2.96 |
/// | 英文散文 | 6.12 |
///
/// 所以这里取最密的 1.68：**宁可高估**。低估会让请求被 provider 打回，
/// 高估只是提前提示开新会话。代价不对称。
///
/// 代价是对英文散文高估约 3.6 倍。这可以接受，因为**循环内的闸门不用它**——
/// 循环里用 API 报回来的真实 `prompt_tokens`（见 `run_turn`）。
/// 这个函数只在两个地方兜底：入口检查已有历史、以及估算本轮新增的部分。
fn est_tokens(s: &str) -> usize {
    let (mut cjk, mut ascii) = (0usize, 0usize);
    for c in s.chars() {
        // 0x2E80 往上是 CJK 部首、汉字、假名、全角标点这一大片
        if c as u32 >= 0x2E80 {
            cjk += 1;
        } else {
            ascii += 1;
        }
    }
    cjk * 100 / 185 + ascii * 100 / 168
}

fn est_messages(msgs: &[Msg]) -> usize {
    msgs.iter()
        .map(|m| match m {
            Msg::User(t) => est_tokens(t),
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                est_tokens(text)
                    + tool_calls
                        .iter()
                        .map(|c| est_tokens(&c.args.to_string()) + 8)
                        .sum::<usize>()
            }
            Msg::Tool(r) => est_tokens(&r.content) + 8,
        })
        .sum()
}

const CONVERGENCE_NOTICE: &str = "\
[运行时提示] 你已经用掉大部分可用轮次，现在开始收敛：优先完成还没做完的必要工作，\
不要再做可选的探索或重复检查。做不完就明确说清卡在哪，**不许声称工具结果不支持的成果**。";

const BUDGET_EXHAUSTED: &str =
    "外部调用预算已用尽，这个工具没有执行。请用已经拿到的信息作答，并说明哪部分没查成。";

/// 跑完一次提问。
///
/// **永远返回 TurnResult，不返回 Err。** 所有失败都表达在 `outcome` 里，
/// 因为无论怎么失败，`items` 都必须是配对完整的——调用方要拿它落库。
pub fn run_turn(
    llm: &dyn LlmClient,
    api: &dyn DiscoveryApi,
    store: &mut SqliteStore,
    history: &[Item],
    question: &str,
    cfg: &LoopConfig,
) -> TurnResult {
    // 两本账。settle() 是唯一同时改它们的地方——分开改的话，
    // 漏更新其中一本会产生很隐蔽的 bug（见 settle 的注释）。
    let mut items: Vec<Item> = Vec::new();
    let mut messages: Vec<Msg> = crate::agent::context::to_messages(history);

    let mut idx = 1i64;
    items.push(Item::user_message(idx, question));
    idx += 1;
    messages.push(Msg::user(question));

    let mut res = TurnResult {
        outcome: TurnOutcome::Done,
        answer: String::new(),
        items,
        iterations: 0,
        tool_calls_made: 0,
        credits_charged: 0,
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
    };

    // ★ 入口闸门：--continue 攒出来的历史可能一上来就超。
    //   在发请求之前拦，不花钱，并且能给用户一句能行动的话。
    let used = est_messages(&messages);
    if used > cfg.context_budget_tokens {
        res.outcome = TurnOutcome::ContextBudget {
            used,
            limit: cfg.context_budget_tokens,
        };
        return res;
    }

    let tools = tool_defs();
    let mut iteration = 0usize;
    // 上次模型调用报回的真实 prompt_tokens，以及当时 messages 的长度。
    // 「当前上下文有多长」= 这个真实值 + 之后新追加那几条的估算，
    // 比整段重新估准得多。
    let (mut real_prompt_tokens, mut settled_msgs);

    loop {
        // ── CallingModel 入口 ────────────────────────────────
        iteration += 1;
        if iteration > cfg.max_iterations {
            res.outcome = TurnOutcome::IterationCap;
            return res; // ★ 超限那一轮的请求根本不发，不白花钱
        }
        if iteration == cfg.convergence_iteration {
            messages.push(Msg::user(CONVERGENCE_NOTICE));
        }
        res.iterations = iteration;

        let resp = match llm.complete(&ModelRequest {
            system: SYSTEM_PROMPT.to_string(),
            messages: messages.clone(),
            max_tokens: llm.max_tokens_limit(),
            tools: tools.clone(),
        }) {
            Ok(r) => r,
            Err(e) => {
                res.outcome = TurnOutcome::ModelError(e.to_string());
                return res;
            }
        };
        res.input_tokens += resp.input_tokens;
        res.cached_input_tokens += resp.cached_input_tokens;
        // ★ 这一轮请求的**真实**长度，provider 亲口报的。
        //   后面判断上下文预算用它当基准，而不是拿字符去猜——
        //   字符估算对英文能差 3.6 倍，而这个数是准的。
        real_prompt_tokens = resp.input_tokens as usize;
        settled_msgs = messages.len();
        res.output_tokens += resp.output_tokens;

        // ── settle：两本账一起更新 ───────────────────────────
        settle_assistant(
            &mut res.items,
            &mut messages,
            &mut idx,
            iteration as i64,
            &resp,
        );

        // ★ 终止条件只看 tool_calls 是否为空，**不看 text**。
        //   实测 DeepSeek 发起工具调用时常常同时说一句话，
        //   拿 text 判断第一轮就会误判结束。
        if resp.tool_calls.is_empty() {
            res.answer = resp.text;
            res.outcome = TurnOutcome::Done;
            return res;
        }

        // ── ExecutingTools：唯一出边是回到 CallingModel ──────
        for call in &resp.tool_calls {
            let over_calls = res.tool_calls_made >= cfg.max_tool_calls;
            // 真实基准 + 只估算本轮新追加的那几条
            let projected =
                real_prompt_tokens + est_messages(&messages[settled_msgs.min(messages.len())..]);
            let over_context = projected >= cfg.context_budget_tokens;

            let (content, is_error, raw, endpoint, credits) = if over_calls || over_context {
                // ★ 预算用尽也**必须**产出配对的结果，不能跳过。
                (BUDGET_EXHAUSTED.to_string(), true, None, None, Some(0))
            } else {
                let out = execute(api, store, call);
                res.tool_calls_made += 1;
                res.credits_charged += out.credits_charged.unwrap_or(0);
                (
                    out.result.content,
                    out.result.is_error,
                    out.raw_json,
                    out.endpoint,
                    out.credits_charged,
                )
            };

            res.items.push(Item::function_call_output_full(
                idx,
                iteration as i64,
                &call.id,
                &content,
                is_error,
                raw,
                endpoint.as_deref(),
                credits,
            ));
            idx += 1;
            messages.push(Msg::Tool(ToolResult {
                call_id: call.id.clone(),
                content,
                is_error,
            }));
        }
    }
}

/// 一轮模型输出的结算：存档和模型上下文**在同一个函数里一起更新**。
///
/// 分开写的话有四个地方要各改两遍，漏一个就出两种隐蔽 bug：
/// 只改存档 → 模型下一轮看不到结果，会重复调同一个工具；
/// 只改上下文 → 库里缺一段，下次 `--continue` 有洞、配对检查报孤儿。
fn settle_assistant(
    items: &mut Vec<Item>,
    messages: &mut Vec<Msg>,
    idx: &mut i64,
    iteration: i64,
    resp: &crate::agent::llm::ModelResponse,
) {
    items.push(Item::assistant_message_full(
        *idx,
        iteration,
        &resp.text,
        &resp.reasoning,
    ));
    *idx += 1;
    // ★ idx 在这里就按开单顺序全部分配好，不等执行完。
    //   以后改并发时，谁先完成无所谓，各填各的槽位——
    //   按完成顺序排会让历史顺序和模型开单顺序不一致。
    for call in &resp.tool_calls {
        items.push(Item::function_call(
            *idx, iteration, &call.id, &call.name, &call.args,
        ));
        *idx += 1;
    }
    messages.push(Msg::Assistant {
        text: resp.text.clone(),
        reasoning: resp.reasoning.clone(),
        tool_calls: resp.tool_calls.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::llm::{LlmClient, ModelResponse, Pricing, StopReason, ToolCall};
    use crate::content::model::{FetchedVideo, ItemKind};
    use crate::error::{ClipKnowError, Result};
    use crate::ingest::discovery::{DiscoveryApi, Endpoint, RawResponse};
    use crate::ingest::url::{ParsedUrl, Platform};
    use crate::store::sqlite::SqliteStore;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// 按脚本返回预设响应。不联网、不花钱，每条状态转移都测得到。
    /// 这是第一版把 LlmClient 切成 trait 换来的直接好处。
    struct MockLlm {
        script: RefCell<VecDeque<ModelResponse>>,
        seen: RefCell<Vec<ModelRequest>>,
    }

    impl MockLlm {
        fn new(script: Vec<ModelResponse>) -> Self {
            MockLlm {
                script: RefCell::new(script.into()),
                seen: RefCell::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.seen.borrow().len()
        }
    }

    impl LlmClient for MockLlm {
        fn complete(&self, req: &ModelRequest) -> Result<ModelResponse> {
            self.seen.borrow_mut().push(req.clone());
            self.script
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| ClipKnowError::Llm("脚本用完了：循环比预期多跑了一轮".into()))
        }
        fn pricing(&self) -> Pricing {
            Pricing {
                input_per_mtok: 0.0,
                cached_input_per_mtok: 0.0,
                output_per_mtok: 0.0,
            }
        }
        fn model_name(&self) -> &str {
            "mock"
        }
        fn max_tokens_limit(&self) -> u32 {
            8192
        }
    }

    fn says(text: &str) -> ModelResponse {
        ModelResponse {
            text: text.into(),
            reasoning: String::new(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 5,
        }
    }

    fn wants(text: &str, calls: &[(&str, &str)]) -> ModelResponse {
        ModelResponse {
            text: text.into(),
            reasoning: String::new(),
            tool_calls: calls
                .iter()
                .enumerate()
                .map(|(i, (name, q))| ToolCall {
                    id: format!("call_{i:02}_x"),
                    name: (*name).into(),
                    args: json!({"platform": "youtube", "query": q}),
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 5,
        }
    }

    #[derive(Default)]
    struct FakeApi {
        calls: RefCell<usize>,
        payload_chars: usize,
    }

    impl DiscoveryApi for FakeApi {
        fn call(&self, _: Endpoint, p: Platform, arg: &str) -> Result<RawResponse> {
            *self.calls.borrow_mut() += 1;
            // 造出指定长度的标题，用来撑上下文
            let title = if self.payload_chars > 0 {
                "啊".repeat(self.payload_chars)
            } else {
                format!("搜到的第一条（{arg}）")
            };
            Ok(RawResponse {
                endpoint: format!("/fake/{}", p.as_str()),
                body: json!({"videos": [{"id": "v1", "title": title,
                    "channel": {"title": "某博主", "handle": "somebody", "id": "UC1"}}]}),
            })
        }
        fn fetch_video(&self, _: &ParsedUrl, _: &str) -> Result<FetchedVideo> {
            panic!("这些用例不该走到 fetch_video")
        }
    }

    fn cfg() -> LoopConfig {
        LoopConfig::default()
    }
    fn st() -> SqliteStore {
        SqliteStore::in_memory().unwrap()
    }

    // -----------------------------------------------------------------

    #[test]
    fn a_question_with_no_tool_calls_finishes_in_one_iteration() {
        let llm = MockLlm::new(vec![says("这是答案")]);
        let out = run_turn(&llm, &FakeApi::default(), &mut st(), &[], "问题", &cfg());

        assert!(matches!(out.outcome, TurnOutcome::Done));
        assert_eq!(out.answer, "这是答案");
        assert_eq!(out.iterations, 1);
        assert_eq!(llm.calls(), 1);
    }

    #[test]
    fn the_loop_ends_on_empty_tool_calls_not_on_empty_text() {
        // 实测 DeepSeek 发起工具调用时常常同时说一句话。
        // 拿 text 判断结束，第一轮就会误判：工具一个都没执行，
        // 拿到的「答案」是一句「我来帮你搜索」。
        let llm = MockLlm::new(vec![
            wants("我来帮你搜索", &[("search_videos", "科普")]),
            says("真正的答案"),
        ]);
        let api = FakeApi::default();
        let out = run_turn(&llm, &api, &mut st(), &[], "问题", &cfg());

        assert_eq!(out.answer, "真正的答案", "不能把第一轮那句话当答案");
        assert_eq!(*api.calls.borrow(), 1, "工具必须真的执行了");
        assert_eq!(out.iterations, 2);
    }

    #[test]
    fn settle_updates_both_the_archive_and_the_model_context() {
        // 两本账必须一起更新。只更新存档 → 模型下一轮看不到工具结果，
        // 会重复调同一个工具；只更新上下文 → 库里缺一段，下次 --continue 有洞。
        let llm = MockLlm::new(vec![
            wants("我搜一下", &[("search_videos", "科普")]),
            says("答案"),
        ]);
        let out = run_turn(&llm, &FakeApi::default(), &mut st(), &[], "问题", &cfg());

        // 存档：问题 + assistant + call + output + 终答 = 5 条
        let kinds: Vec<ItemKind> = out.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::UserMessage,
                ItemKind::AssistantMessage,
                ItemKind::FunctionCall,
                ItemKind::FunctionCallOutput,
                ItemKind::AssistantMessage,
            ]
        );

        // 模型上下文：第二轮的请求里必须能看到第一轮的工具结果
        let second = &llm.seen.borrow()[1];
        assert_eq!(second.messages.len(), 3, "user + assistant(带工具) + tool");
        assert!(matches!(&second.messages[2], Msg::Tool(_)));
    }

    #[test]
    fn several_tool_calls_in_one_iteration_all_get_executed_and_paired() {
        let llm = MockLlm::new(vec![
            wants(
                "同时搜三个词",
                &[
                    ("search_videos", "科普"),
                    ("search_videos", "科学"),
                    ("search_videos", "冷知识"),
                ],
            ),
            says("答案"),
        ]);
        let api = FakeApi::default();
        let out = run_turn(&llm, &api, &mut st(), &[], "问题", &cfg());

        assert_eq!(*api.calls.borrow(), 3);
        let calls = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCall)
            .count();
        let outs = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCallOutput)
            .count();
        assert_eq!((calls, outs), (3, 3), "每个请求都要有配对的结果");
        // 顺序：三个 call 连着，然后三个 output 连着，按开单顺序
        let ids: Vec<&str> = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCallOutput)
            .map(|i| i.call_id.as_deref().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["call_00_x", "call_01_x", "call_02_x"],
            "顺序按开单顺序，不按完成顺序"
        );
    }

    #[test]
    fn iterations_are_capped_and_the_over_limit_request_is_never_sent() {
        // 超限那一轮的请求根本不发出去，不白花钱
        let script: Vec<ModelResponse> = (0..40)
            .map(|i| wants("再搜", &[("search_videos", &format!("词{i}"))]))
            .collect();
        let llm = MockLlm::new(script);
        let out = run_turn(&llm, &FakeApi::default(), &mut st(), &[], "问题", &cfg());

        assert!(matches!(out.outcome, TurnOutcome::IterationCap));
        assert_eq!(llm.calls(), cfg().max_iterations, "第 11 轮不该发请求");
    }

    #[test]
    fn a_convergence_notice_is_injected_before_the_hard_cap() {
        // 硬砍是「到点就死」；提前给模型预算感知，它通常自己就收尾了
        let script: Vec<ModelResponse> = (0..40)
            .map(|_| wants("再搜", &[("search_videos", "词")]))
            .collect();
        let llm = MockLlm::new(script);
        run_turn(&llm, &FakeApi::default(), &mut st(), &[], "问题", &cfg());

        let seen = llm.seen.borrow();
        let at_convergence = &seen[cfg().convergence_iteration - 1];
        let has_notice = at_convergence
            .messages
            .iter()
            .any(|m| matches!(m, Msg::User(t) if t.contains("收敛")));
        assert!(
            has_notice,
            "第 {} 轮该插入收敛提示",
            cfg().convergence_iteration
        );
        assert!(
            !seen[0]
                .messages
                .iter()
                .any(|m| matches!(m, Msg::User(t) if t.contains("收敛"))),
            "第一轮不该有"
        );
    }

    #[test]
    fn external_calls_are_capped_and_remaining_tools_still_get_paired_results() {
        // 预算用尽**不能**跳过剩下的工具：每个 call 仍要产出结果，
        // 内容写「预算已用尽」。ExecutingTools 只有一条出边。
        let many: Vec<(&str, &str)> = (0..4).map(|_| ("search_videos", "词")).collect();
        let script: Vec<ModelResponse> = (0..12).map(|_| wants("搜", &many)).collect();
        let llm = MockLlm::new(script);
        let api = FakeApi::default();
        let out = run_turn(&llm, &api, &mut st(), &[], "问题", &cfg());

        assert!(
            *api.calls.borrow() <= cfg().max_tool_calls,
            "SC 调用不该超上限"
        );
        let calls = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCall)
            .count();
        let outs = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCallOutput)
            .count();
        assert_eq!(calls, outs, "预算用尽也必须每个都配对");
        assert!(
            out.items
                .iter()
                .any(|i| i.kind == ItemKind::FunctionCallOutput
                    && i.payload["content"].as_str().unwrap_or("").contains("预算")),
            "要明确告诉模型是预算用尽，不是工具坏了"
        );
    }

    #[test]
    fn an_oversized_existing_history_is_refused_before_the_first_request() {
        // 连续追问攒出来的历史，第一次模型调用就会超。
        // 要在发请求之前拦，不花钱，并且报清楚。
        let huge = Item::user_message(1, &"啊".repeat(2_000_000)); // 约 108 万 token，超过 1M 窗口
        let llm = MockLlm::new(vec![says("不该被调用")]);
        let out = run_turn(
            &llm,
            &FakeApi::default(),
            &mut st(),
            &[huge],
            "再问",
            &cfg(),
        );

        match out.outcome {
            TurnOutcome::ContextBudget { used, limit } => {
                assert!(used > limit, "used={used} limit={limit}");
            }
            other => panic!("应该是 ContextBudget，实际 {other:?}"),
        }
        assert_eq!(llm.calls(), 0, "不该发请求");
    }

    #[test]
    fn tool_output_that_would_blow_the_budget_stops_further_tool_calls() {
        // 跑着跑着超：一次搜索实测约 3100 token，
        // 10 轮 × 每轮 3 个 = 93000 token，超过 64K 窗口。
        // 迭代次数管不住上下文长度，这是两个独立维度。
        let script: Vec<ModelResponse> = (0..6)
            .map(|_| wants("搜", &[("search_videos", "词")]))
            .collect();
        let llm = MockLlm::new(script);
        let api = FakeApi {
            payload_chars: 200_000,
            ..Default::default()
        };
        let out = run_turn(&llm, &api, &mut st(), &[], "问题", &cfg());

        assert!(
            *api.calls.borrow() < 12,
            "撑爆之前该停下，实际打了 {} 次",
            api.calls.borrow()
        );
        let calls = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCall)
            .count();
        let outs = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCallOutput)
            .count();
        assert_eq!(calls, outs, "配对不能破");
    }

    #[test]
    fn a_model_error_becomes_a_failed_outcome_with_paired_history() {
        let llm = MockLlm::new(vec![]); // 脚本空的 → 第一次调用就报错
        let out = run_turn(&llm, &FakeApi::default(), &mut st(), &[], "问题", &cfg());

        assert!(matches!(out.outcome, TurnOutcome::ModelError(_)));
        let calls = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCall)
            .count();
        let outs = out
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::FunctionCallOutput)
            .count();
        assert_eq!(calls, outs);
    }

    #[test]
    fn a_failing_tool_does_not_abort_the_loop() {
        // 模型编了个不存在的工具名。要让它看到错误自己改，不是崩掉。
        let llm = MockLlm::new(vec![
            wants("我试试", &[("search_the_web", "科普")]),
            says("好吧我用别的方法"),
        ]);
        let out = run_turn(&llm, &FakeApi::default(), &mut st(), &[], "问题", &cfg());

        assert!(matches!(out.outcome, TurnOutcome::Done));
        assert_eq!(out.answer, "好吧我用别的方法");
        assert!(
            out.items
                .iter()
                .any(|i| i.kind == ItemKind::FunctionCallOutput && i.payload["is_error"] == true),
            "失败要标成 is_error 回传"
        );
    }

    #[test]
    fn existing_history_is_prepended_so_a_follow_up_sees_it() {
        let history = vec![
            Item::user_message(1, "上次问的"),
            Item::assistant_message(2, 1, "上次答的"),
        ];
        let llm = MockLlm::new(vec![says("这次答的")]);
        run_turn(
            &llm,
            &FakeApi::default(),
            &mut st(),
            &history,
            "这次问的",
            &cfg(),
        );

        let first = &llm.seen.borrow()[0];
        assert_eq!(first.messages.len(), 3, "两条旧的 + 一条新问题");
        assert!(matches!(&first.messages[0], Msg::User(t) if t == "上次问的"));
        assert!(matches!(&first.messages[2], Msg::User(t) if t == "这次问的"));
    }

    #[test]
    fn item_indices_are_continuous_and_start_after_the_existing_history() {
        // idx 是我们自己生成的、可以依赖的编号（call_id 不是）。
        // 续会话时不能从 1 重来，否则 UNIQUE(turn_id, idx) 之外的顺序会乱。
        let history = vec![
            Item::user_message(1, "旧"),
            Item::assistant_message(2, 1, "旧"),
        ];
        let llm = MockLlm::new(vec![says("答")]);
        let out = run_turn(&llm, &FakeApi::default(), &mut st(), &history, "新", &cfg());

        let idxs: Vec<i64> = out.items.iter().map(|i| i.idx).collect();
        assert_eq!(
            idxs,
            vec![1, 2],
            "新 turn 的 idx 从 1 开始（idx 是 turn 内的）"
        );
    }

    #[test]
    fn iteration_numbers_restart_at_one_for_each_question() {
        // 上限是针对「回答一个问题」设的，所以新提问要从 1 重新数
        let history = vec![Item::assistant_message(9, 7, "上个 turn 跑到第 7 轮")];
        let llm = MockLlm::new(vec![wants("搜", &[("search_videos", "词")]), says("答")]);
        let out = run_turn(
            &llm,
            &FakeApi::default(),
            &mut st(),
            &history,
            "新问题",
            &cfg(),
        );

        let iters: Vec<Option<i64>> = out.items.iter().map(|i| i.iteration).collect();
        assert_eq!(iters[1], Some(1), "新 turn 的第一轮该是 1，不是 8");
    }

    // -----------------------------------------------------------------
    // token 估算：2026-08-19 拿 deepseek-v4-flash 实测的真实 token 数
    // -----------------------------------------------------------------

    /// 每条都是实测值：内容 → API 报的 prompt_tokens。
    fn measured() -> Vec<(&'static str, String, usize)> {
        vec![
            ("纯中文口播", "今天我们来聊聊一个特别有意思的物理现象，为什么摇过的可乐一打开就会喷出来。".repeat(40), 799),
            ("纯英文散文", "Today we are going to talk about a very interesting physical phenomenon involving carbonated beverages. ".repeat(30), 510),
            ("中英混排（渲染后的搜索结果）", "1. 【千萬不要吃】人類不能吃生肉的真正原因\n   作者 老高與小茉 Mr & Mrs Gao (@laogao) | 播放 4780866 | 赞 59762 | 时长 25:28\n   https://www.youtube.com/watch?v=rCJX4pPz1_A\n".repeat(25), 1799),
            ("JSON / 数字", r#"{"aweme_id":"7673502248126663954","play_count":96872,"digg_count":980},"#.repeat(40), 960),
        ]
    }

    #[test]
    fn the_estimator_never_underestimates_by_more_than_five_percent() {
        // 这是唯一真正要紧的性质：低估会让请求被 provider 打回，
        // 高估只是提前拦下、浪费一点额度。代价不对称，所以宁可高估。
        for (name, text, real) in measured() {
            let est = est_tokens(&text);
            assert!(
                est as f64 >= real as f64 * 0.95,
                "{name}：估 {est}，实际 {real}，低估了 {:.0}%",
                (1.0 - est as f64 / real as f64) * 100.0
            );
        }
    }

    #[test]
    fn the_estimator_is_accurate_on_the_content_this_project_actually_sees() {
        // 实际会遇到的是中文和「中文+句柄+数字+URL」混排，这两个要准。
        // 高估多少可以接受？ASCII 密度实测跨度 1.68～6.12，取最密的 1.68 后：
        //   JSON 高估 1.8 倍、纯英文散文高估 3.6 倍。
        // 这是保守取值必然的代价，换来的是「绝不低估」。
        for (name, text, real) in measured() {
            let est = est_tokens(&text);
            let ratio = est as f64 / real as f64;
            let tolerance = match name {
                "纯中文口播" | "中英混排（渲染后的搜索结果）" => 1.3,
                _ => 4.0,
            };
            assert!(
                ratio < tolerance,
                "{name}：估 {est}，实际 {real}，高估 {ratio:.1} 倍（容忍 {tolerance}）"
            );
        }
    }

    #[test]
    fn the_in_loop_gate_uses_the_real_token_count_not_the_estimate() {
        // 循环里不该拿字符去猜——API 每次都报真实 prompt_tokens。
        // 造一个「模型报了个很大的真实值」的场景：闸门必须据此停下，
        // 哪怕 messages 里的字符数看起来很少。
        let mut big = says("答案");
        big.input_tokens = 999_999; // provider 说这轮输入很大
        let llm = MockLlm::new(vec![
            ModelResponse {
                input_tokens: 999_999,
                ..wants("搜", &[("search_videos", "词")])
            },
            big,
        ]);
        let api = FakeApi::default();
        run_turn(&llm, &api, &mut st(), &[], "问题", &cfg());

        assert_eq!(
            *api.calls.borrow(),
            0,
            "真实 token 数已超预算，不该再执行工具"
        );
    }
}

//! 大模型客户端。
//!
//! **这个文件是整个项目里唯一知道「我们在用哪家模型」的地方。**
//!
//! 关键约束：`ModelRequest` / `ModelResponse` 是我们自己定义的中立类型，
//! 不是 Anthropic 的 JSON 结构。程序其余部分只认识这两个类型，压根不知道
//! 背后是谁。以后换 DeepSeek，只需要在这个文件里加一个 `DeepSeekClient`，
//! main.rs、store、ingest 一行都不用改。
//!
//! （反面教材是直接把 Anthropic 返回的 `content: [block]` 结构传遍全程序——
//! DeepSeek 根本没有 block 这个概念，那样迁移就得翻整个项目。）
//!
//! Rust 没有官方 Anthropic SDK，所以这里是手写 HTTP。好在有 reqwest + serde，
//! 总共不到 100 行。

use serde_json::{json, Value};

use crate::error::{ClipKnowError, Result};

/// 默认模型。Claude Opus 5。
pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// 非流式请求的输出上限。
///
/// 注意 Opus 5 **默认开启思考**，而 max_tokens 是「思考 + 回答」的总上限，
/// 所以要留足余量，否则回答会被从中间截断。
/// 另外非流式请求超过约 16000 会撞 HTTP 超时。
pub const DEFAULT_MAX_TOKENS: u32 = 16000;

// ---------------------------------------------------------------------------
// 中立类型：不属于任何一家厂商
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub role: Role,
    pub text: String,
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Self {
        Msg { role: Role::User, text: text.into() }
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<Msg>,
    pub max_tokens: u32,
}

/// 模型为什么停下来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 正常说完了
    EndTurn,
    /// 撞到 max_tokens 被截断了
    MaxTokens,
    /// 安全分类器拒绝了，此时没有正文
    Refusal,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub text: String,
    pub stop_reason: StopReason,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// 每百万 token 的价格（美元）。各家自己报，不写死在通用代码里。
///
/// ⚠️ 价格会变，这里的数字只用于给你一个数量级的感觉，
/// 真要对账请以各家官网为准。
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl Pricing {
    pub fn cost_usd(&self, input_tokens: u32, output_tokens: u32) -> f64 {
        input_tokens as f64 / 1_000_000.0 * self.input_per_mtok
            + output_tokens as f64 / 1_000_000.0 * self.output_per_mtok
    }
}

/// 所有大模型供应商都实现这个 trait。
///
/// 注意这里出现的类型全是我们自己的：`ModelRequest`、`ModelResponse`、`Pricing`。
/// 没有任何一家厂商的概念泄漏进来——这就是为什么加一家新供应商
/// 只需要动这个文件。
pub trait LlmClient {
    fn complete(&self, req: &ModelRequest) -> Result<ModelResponse>;

    /// 这家的报价，用来估算成本。
    fn pricing(&self) -> Pricing;

    /// 显示给用户看的名字，比如 "claude-opus-5"。
    fn model_name(&self) -> &str;

    /// 这家能接受的 max_tokens 上限。各家差别很大：
    /// Anthropic 非流式约 16000，DeepSeek 是 8192。
    fn max_tokens_limit(&self) -> u32;
}

// ---------------------------------------------------------------------------
// Anthropic 实现
// ---------------------------------------------------------------------------

pub struct AnthropicClient {
    http: reqwest::blocking::Client,
    api_key: String,
    model: String,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String) -> Self {
        let http = reqwest::blocking::Client::builder()
            // 模型思考可能很久，超时给宽一点
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("构造 HTTP 客户端失败");
        Self { http, api_key, model }
    }

    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ClipKnowError::MissingEnv("ANTHROPIC_API_KEY"))?;
        // 每家一个独立的变量。共用一个 CLIPKNOW_MODEL 会出事：
        // 设了 claude-opus-5 之后再 --provider deepseek，模型名会被发给
        // DeepSeek，直接 400。
        let model =
            std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(Self::new(key, model))
    }

    /// 把中立的 ModelRequest 翻译成 Anthropic 的请求体。
    fn to_anthropic_body(&self, req: &ModelRequest) -> Value {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| {
                json!({
                    "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                    "content": m.text,
                })
            })
            .collect();

        // 注意这里没有 temperature / top_p / top_k——
        // Opus 5 上这三个参数已被移除，传了直接 400。
        json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "system": req.system,
            "messages": messages,
        })
    }
}

impl LlmClient for AnthropicClient {
    fn pricing(&self) -> Pricing {
        // Claude Opus 5：$5 / $25 每百万 token
        Pricing { input_per_mtok: 5.0, output_per_mtok: 25.0 }
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_tokens_limit(&self) -> u32 {
        // 非流式请求再高会撞 HTTP 超时
        16_000
    }

    fn complete(&self, req: &ModelRequest) -> Result<ModelResponse> {
        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&self.to_anthropic_body(req))
            .send()?;

        let status = resp.status();
        let body: Value = resp.json()?;

        if !status.is_success() {
            let msg = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("(响应里没有 error.message)");
            return Err(ClipKnowError::Llm(format!("HTTP {status}: {msg}")));
        }

        parse_anthropic_response(&body)
    }
}

/// 解析 Anthropic 的响应。单独拆成函数，方便用假数据测试，不用真的发请求。
fn parse_anthropic_response(body: &Value) -> Result<ModelResponse> {
    let stop_reason = match body.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("missing".into()),
    };

    // ★ 必须先判断 refusal 再读 content。
    // 被拒绝时 content 是空数组，直接索引 content[0] 在 Rust 里是 panic 不是异常，
    // 程序会当场崩掉。
    if stop_reason == StopReason::Refusal {
        return Err(ClipKnowError::LlmRefusal);
    }

    // Opus 5 默认开启思考，所以 content 数组里除了 text 块还可能有 thinking 块。
    // 只挑 type == "text" 的拼起来。
    let text = body
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let usage = body.get("usage");
    let get_u32 = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };

    Ok(ModelResponse {
        text,
        stop_reason,
        input_tokens: get_u32("input_tokens"),
        output_tokens: get_u32("output_tokens"),
    })
}

// ---------------------------------------------------------------------------
// DeepSeek 实现
//
// DeepSeek 用的是「OpenAI 兼容」格式——路径、字段名、返回结构都照抄 OpenAI，
// 所以这套代码稍微改个 base_url 和模型名，也能用在通义千问、智谱、豆包、
// Kimi 上。真正格式独一份的反而是 Anthropic。
// ---------------------------------------------------------------------------

pub const DEEPSEEK_DEFAULT_MODEL: &str = "deepseek-chat";

pub struct DeepSeekClient {
    http: reqwest::blocking::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl DeepSeekClient {
    pub fn new(api_key: String, model: String) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("构造 HTTP 客户端失败");
        Self {
            http,
            api_key,
            model,
            base_url: "https://api.deepseek.com".to_string(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| ClipKnowError::MissingEnv("DEEPSEEK_API_KEY"))?;
        let model = std::env::var("DEEPSEEK_MODEL")
            .unwrap_or_else(|_| DEEPSEEK_DEFAULT_MODEL.to_string());
        Ok(Self::new(key, model))
    }

    /// 把同一个 ModelRequest 翻译成 OpenAI 格式。
    ///
    /// 对比 Anthropic 版本看差异：
    /// - system 不是顶层字段，而是 messages 里 role="system" 的第一条
    /// - 其余结构基本一致
    fn to_openai_body(&self, req: &ModelRequest) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": req.system })];
        messages.extend(req.messages.iter().map(|m| {
            json!({
                "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                "content": m.text,
            })
        }));

        json!({
            "model": self.model,
            "max_tokens": req.max_tokens.min(self.max_tokens_limit()),
            "messages": messages,
            "stream": false,
        })
    }
}

impl LlmClient for DeepSeekClient {
    fn pricing(&self) -> Pricing {
        // ⚠️ DeepSeek 调过几次价，这里是数量级参考，以官网为准：
        // https://platform.deepseek.com/api-docs/pricing
        Pricing { input_per_mtok: 0.27, output_per_mtok: 1.10 }
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_tokens_limit(&self) -> u32 {
        // deepseek-chat 的输出上限，比 Anthropic 低得多
        8192
    }

    fn complete(&self, req: &ModelRequest) -> Result<ModelResponse> {
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            // ★ 和 Anthropic 的第一个差异：Bearer 认证，不是 x-api-key
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&self.to_openai_body(req))
            .send()?;

        let status = resp.status();
        let body: Value = resp.json()?;

        if !status.is_success() {
            let msg = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("(响应里没有 error.message)");
            return Err(ClipKnowError::Llm(format!("HTTP {status}: {msg}")));
        }

        parse_openai_response(&body)
    }
}

/// 解析 OpenAI 格式的响应。
fn parse_openai_response(body: &Value) -> Result<ModelResponse> {
    let choice = body
        .pointer("/choices/0")
        .ok_or_else(|| ClipKnowError::Llm("响应里没有 choices[0]".into()))?;

    // ★ 第二个差异：叫 finish_reason，取值也和 Anthropic 不同
    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("missing".into()),
    };

    if stop_reason == StopReason::Refusal {
        return Err(ClipKnowError::LlmRefusal);
    }

    // ★ 第三个差异：正文是一个字符串，不是 block 数组
    let text = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // ★ 第四个差异：usage 字段名叫 prompt_tokens / completion_tokens
    let usage = body.get("usage");
    let get_u32 = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };

    Ok(ModelResponse {
        text,
        stop_reason,
        input_tokens: get_u32("prompt_tokens"),
        output_tokens: get_u32("completion_tokens"),
    })
}

// ---------------------------------------------------------------------------
// 按环境变量挑一家
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    DeepSeek,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Provider> {
        match s.to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Some(Provider::Anthropic),
            "deepseek" => Some(Provider::DeepSeek),
            _ => None,
        }
    }
}

/// 造一个大模型客户端。
///
/// 没有显式指定时按这个顺序挑：设了哪家的 key 就用哪家，
/// 两家都设了优先 DeepSeek（便宜，适合开发调试）。
///
/// 返回的是 `Box<dyn LlmClient>`——相当于 C++ 的 `unique_ptr<ILlmClient>`，
/// 调用方拿到之后并不知道底下是哪一家。
pub fn build_client(explicit: Option<Provider>) -> Result<Box<dyn LlmClient>> {
    let has_deepseek = std::env::var("DEEPSEEK_API_KEY").is_ok();
    let has_anthropic = std::env::var("ANTHROPIC_API_KEY").is_ok();

    let chosen = match explicit {
        Some(p) => p,
        None if has_deepseek => Provider::DeepSeek,
        None if has_anthropic => Provider::Anthropic,
        None => return Err(ClipKnowError::MissingEnv("DEEPSEEK_API_KEY 或 ANTHROPIC_API_KEY")),
    };

    Ok(match chosen {
        Provider::Anthropic => Box::new(AnthropicClient::from_env()?),
        Provider::DeepSeek => Box::new(DeepSeekClient::from_env()?),
    })
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_normal_response() {
        let body = json!({
            "content": [{"type": "text", "text": "这个视频在讲早期获客。"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5000, "output_tokens": 800}
        });
        let r = parse_anthropic_response(&body).unwrap();
        assert_eq!(r.text, "这个视频在讲早期获客。");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!(r.input_tokens, 5000);
        assert_eq!(r.output_tokens, 800);
    }

    #[test]
    fn skips_thinking_blocks_and_keeps_only_text() {
        // Opus 5 默认开启思考，响应里会混进 thinking 块
        let body = json!({
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": "答案第一段。"},
                {"type": "text", "text": "第二段。"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let r = parse_anthropic_response(&body).unwrap();
        assert_eq!(r.text, "答案第一段。第二段。", "thinking 块要跳过，text 块要拼接");
    }

    #[test]
    fn refusal_returns_error_instead_of_panicking_on_empty_content() {
        // 这是最容易写崩的分支：被拒时 content 是空数组
        let body = json!({ "content": [], "stop_reason": "refusal" });
        let err = parse_anthropic_response(&body).unwrap_err();
        assert!(matches!(err, ClipKnowError::LlmRefusal), "实际是: {err}");
    }

    #[test]
    fn detects_truncation() {
        let body = json!({
            "content": [{"type": "text", "text": "说到一半就被"}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1, "output_tokens": 16000}
        });
        let r = parse_anthropic_response(&body).unwrap();
        assert_eq!(r.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn missing_usage_defaults_to_zero_not_error() {
        let body = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn"
        });
        let r = parse_anthropic_response(&body).unwrap();
        assert_eq!(r.input_tokens, 0);
    }

    #[test]
    fn request_body_has_no_sampling_params() {
        // temperature/top_p/top_k 在 Opus 5 上会 400，必须确保我们没传
        let c = AnthropicClient::new("k".into(), DEFAULT_MODEL.into());
        let body = c.to_anthropic_body(&ModelRequest {
            system: "你是助手".into(),
            messages: vec![Msg::user("你好")],
            max_tokens: 16000,
        });
        assert!(body.get("temperature").is_none(), "不能传 temperature");
        assert!(body.get("top_p").is_none(), "不能传 top_p");
        assert!(body.get("top_k").is_none(), "不能传 top_k");
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["system"], "你是助手");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "你好");
    }

    #[test]
    fn cost_matches_opus5_pricing() {
        let c = AnthropicClient::new("k".into(), DEFAULT_MODEL.into());
        let cost = c.pricing().cost_usd(1_000_000, 1_000_000);
        assert!((cost - 30.0).abs() < 1e-9, "百万输入$5 + 百万输出$25 = $30");
    }

    // -----------------------------------------------------------------------
    // DeepSeek：同一个 ModelRequest 翻译成另一种格式
    // -----------------------------------------------------------------------

    #[test]
    fn deepseek_puts_system_prompt_into_messages_array() {
        // 这是和 Anthropic 最大的结构差异：
        // Anthropic 的 system 是顶层字段，OpenAI 格式要塞进 messages 第一条
        let c = DeepSeekClient::new("k".into(), DEEPSEEK_DEFAULT_MODEL.into());
        let body = c.to_openai_body(&ModelRequest {
            system: "你是助手".into(),
            messages: vec![Msg::user("你好")],
            max_tokens: 8192,
        });

        assert!(body.get("system").is_none(), "OpenAI 格式没有顶层 system 字段");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "你是助手");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "你好");
        assert_eq!(body["model"], DEEPSEEK_DEFAULT_MODEL);
    }

    #[test]
    fn deepseek_clamps_max_tokens_to_its_own_limit() {
        // 上层按 Anthropic 的 16000 传进来，DeepSeek 只吃 8192，
        // 必须在这一层夹住，否则请求会被拒
        let c = DeepSeekClient::new("k".into(), DEEPSEEK_DEFAULT_MODEL.into());
        let body = c.to_openai_body(&ModelRequest {
            system: "s".into(),
            messages: vec![Msg::user("u")],
            max_tokens: 16_000,
        });
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn parses_openai_style_response() {
        let body = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "这个视频在讲早期获客。"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5000, "completion_tokens": 800, "total_tokens": 5800}
        });
        let r = parse_openai_response(&body).unwrap();
        assert_eq!(r.text, "这个视频在讲早期获客。");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        // 字段名和 Anthropic 不同：prompt_tokens / completion_tokens
        assert_eq!(r.input_tokens, 5000);
        assert_eq!(r.output_tokens, 800);
    }

    #[test]
    fn openai_length_maps_to_max_tokens() {
        let body = json!({
            "choices": [{"message": {"content": "半句"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 8192}
        });
        assert_eq!(parse_openai_response(&body).unwrap().stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn openai_content_filter_maps_to_refusal() {
        let body = json!({
            "choices": [{"message": {"content": null}, "finish_reason": "content_filter"}]
        });
        let err = parse_openai_response(&body).unwrap_err();
        assert!(matches!(err, ClipKnowError::LlmRefusal), "实际是: {err}");
    }

    #[test]
    fn openai_missing_choices_errors_instead_of_panicking() {
        let err = parse_openai_response(&json!({})).unwrap_err();
        assert!(matches!(err, ClipKnowError::Llm(_)), "实际是: {err}");
    }

    #[test]
    fn openai_null_content_is_empty_not_panic() {
        let body = json!({
            "choices": [{"message": {"content": null}, "finish_reason": "stop"}]
        });
        assert_eq!(parse_openai_response(&body).unwrap().text, "");
    }

    /// 这个测试是整套 provider 隔离设计的证明：
    /// 同一个 ModelRequest 能喂给两家，各自翻译成自己的格式，
    /// 上层代码完全不需要知道差异。
    #[test]
    fn same_request_works_for_both_providers() {
        let req = ModelRequest {
            system: "你是助手".into(),
            messages: vec![Msg::user("你好")],
            max_tokens: 8000,
        };

        let anthropic = AnthropicClient::new("k".into(), DEFAULT_MODEL.into());
        let deepseek = DeepSeekClient::new("k".into(), DEEPSEEK_DEFAULT_MODEL.into());

        let a_body = anthropic.to_anthropic_body(&req);
        let d_body = deepseek.to_openai_body(&req);

        // 同样的 system 提示词，落在两个完全不同的位置
        assert_eq!(a_body["system"], "你是助手");
        assert_eq!(d_body["messages"][0]["content"], "你是助手");

        // 两家都能把用户的话带上
        assert_eq!(a_body["messages"][0]["content"], "你好");
        assert_eq!(d_body["messages"][1]["content"], "你好");
    }

    #[test]
    fn provider_parses_aliases() {
        assert_eq!(Provider::parse("deepseek"), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("DeepSeek"), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("claude"), Some(Provider::Anthropic));
        assert_eq!(Provider::parse("anthropic"), Some(Provider::Anthropic));
        assert_eq!(Provider::parse("gpt"), None);
    }
}

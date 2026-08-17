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

impl ModelResponse {
    /// 这次调用大概花了多少钱（美元）。按 Claude Opus 5 的价格：
    /// 输入 $5 / 百万 token，输出 $25 / 百万 token。
    pub fn cost_usd(&self) -> f64 {
        self.input_tokens as f64 / 1_000_000.0 * 5.0
            + self.output_tokens as f64 / 1_000_000.0 * 25.0
    }
}

/// 所有大模型供应商都实现这个 trait。
pub trait LlmClient {
    fn complete(&self, req: &ModelRequest) -> Result<ModelResponse>;
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
        let model =
            std::env::var("CLIPKNOW_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
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
        let r = ModelResponse {
            text: String::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
        };
        assert!((r.cost_usd() - 30.0).abs() < 1e-9, "百万输入$5 + 百万输出$25 = $30");
    }
}

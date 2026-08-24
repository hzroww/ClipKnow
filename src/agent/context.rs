//! 对话历史的两次转换：库 ↔ 内存 ↔ 发给模型的消息数组。
//!
//! 为什么需要这一层：大模型 API **完全无状态**，每一轮都要把完整历史重发。
//! 所谓「续上次的会话」，物理上就是把 `items` 表里那串读出来、拼成数组发过去，
//! 没有别的机制。

use crate::agent::llm::{Msg, ToolCall, ToolResult};
use crate::content::model::{Item, ItemKind};

/// 库里的条目 → 发给模型的消息数组。
///
/// 不是一一对应：`function_call` 条目要**并进它前面那条 assistant 消息**。
/// 实测 DeepSeek 一轮能返回 3 个 tool_calls，存库时是 3 条独立条目，
/// 还原时必须合成一条 assistant 消息带 3 个 tool_calls——
/// 拆成三条发回去，协议上就对不上了。
pub fn to_messages(items: &[Item]) -> Vec<Msg> {
    let mut out: Vec<Msg> = Vec::new();

    for it in items {
        match it.kind {
            ItemKind::UserMessage => {
                out.push(Msg::User(str_field(it, "text")));
            }
            ItemKind::AssistantMessage => {
                out.push(Msg::Assistant {
                    text: str_field(it, "text"),
                    // 存了但还原时丢掉，等于没存
                    reasoning: str_field(it, "reasoning"),
                    tool_calls: Vec::new(),
                });
            }
            ItemKind::FunctionCall => {
                let call = ToolCall {
                    id: it.call_id.clone().unwrap_or_default(),
                    name: str_field(it, "name"),
                    args: it.payload.get("args").cloned().unwrap_or_default(),
                };
                // 并进上一条 assistant；没有就补一条空的。
                // 模型偶尔 content 为空、只给 tool_calls，历史里就没有那条
                // assistant——但 tool 消息前面必须有对应的 assistant，否则 400。
                match out.last_mut() {
                    Some(Msg::Assistant { tool_calls, .. }) => tool_calls.push(call),
                    _ => out.push(Msg::Assistant {
                        text: String::new(),
                        reasoning: String::new(),
                        tool_calls: vec![call],
                    }),
                }
            }
            ItemKind::FunctionCallOutput => {
                out.push(Msg::Tool(ToolResult {
                    call_id: it.call_id.clone().unwrap_or_default(),
                    content: str_field(it, "content"),
                    is_error: it
                        .payload
                        .get("is_error")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                }));
            }
        }
    }
    out
}

/// 把条目渲染成给摘要模型看的纯文本记录。
///
/// **不做逐条截断。** 参照的实现把工具输出砍到 500 字符，那是被它自己的
/// 256K 窗口逼的；我们阈值 40 万、目标 15 万，前缀约 25 万 token，
/// 而窗口是 1M，放得下。砍了只会让摘要模型看不到候选名单的全貌——
/// 它就总结不出「我核实过这几个人」。
pub fn to_transcript(items: &[Item]) -> String {
    let mut out = String::new();
    for it in items {
        match it.kind {
            ItemKind::UserMessage => {
                out.push_str(&format!("【用户】{}\n", str_field(it, "text")));
            }
            ItemKind::AssistantMessage => {
                let r = str_field(it, "reasoning");
                if !r.is_empty() {
                    out.push_str(&format!("【助手思考】{r}\n"));
                }
                let t = str_field(it, "text");
                if !t.is_empty() {
                    out.push_str(&format!("【助手】{t}\n"));
                }
            }
            ItemKind::FunctionCall => {
                out.push_str(&format!(
                    "【调用工具】{} {}\n",
                    str_field(it, "name"),
                    it.payload
                        .get("args")
                        .map(|a| a.to_string())
                        .unwrap_or_default()
                ));
            }
            ItemKind::FunctionCallOutput => {
                out.push_str(&format!("【工具结果】{}\n", str_field(it, "content")));
            }
        }
    }
    out
}

fn str_field(it: &Item, key: &str) -> String {
    it.payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::llm::Msg;
    use serde_json::json;

    #[test]
    fn a_stored_history_rebuilds_into_the_message_array() {
        let items = vec![
            Item::user_message(1, "帮我找几个做科普的博主"),
            Item::assistant_message(2, 1, "我先搜一下"),
            Item::function_call(3, 1, "call_00_A", "search_videos", &json!({"query":"科普"})),
            Item::function_call_output(4, 1, "call_00_A", "[结果：20 条]", false, None),
            Item::assistant_message(5, 2, "推荐毕导THU"),
        ];
        let msgs = to_messages(&items);

        // 五条 item → 四条消息：function_call 被并进它前面那条 assistant
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], Msg::User(t) if t.contains("科普")));
        match &msgs[1] {
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                assert_eq!(text, "我先搜一下");
                assert_eq!(tool_calls.len(), 1, "工具请求要并进 assistant 消息");
                assert_eq!(tool_calls[0].id, "call_00_A");
                assert_eq!(tool_calls[0].args["query"], "科普");
            }
            other => panic!("应该是 assistant，实际 {other:?}"),
        }
        match &msgs[2] {
            Msg::Tool(r) => {
                assert_eq!(r.call_id, "call_00_A");
                assert_eq!(r.content, "[结果：20 条]");
            }
            other => panic!("应该是 tool，实际 {other:?}"),
        }
    }

    #[test]
    fn several_calls_in_one_iteration_merge_into_a_single_assistant_message() {
        // 实测 DeepSeek 一轮能返回 3 个 tool_calls。存库时是 3 条 item，
        // 还原时必须合成**一条** assistant 消息带 3 个 tool_calls ——
        // 拆成三条发回去，协议上就对不上了。
        let items = vec![
            Item::assistant_message(1, 1, "我同时搜三个平台"),
            Item::function_call(2, 1, "c1", "search_videos", &json!({})),
            Item::function_call(3, 1, "c2", "search_videos", &json!({})),
            Item::function_call(4, 1, "c3", "search_videos", &json!({})),
            Item::function_call_output(5, 1, "c1", "a", false, None),
            Item::function_call_output(6, 1, "c2", "b", false, None),
            Item::function_call_output(7, 1, "c3", "c", false, None),
        ];
        let msgs = to_messages(&items);

        assert_eq!(msgs.len(), 4, "1 条 assistant + 3 条 tool");
        match &msgs[0] {
            Msg::Assistant { tool_calls, .. } => assert_eq!(tool_calls.len(), 3),
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn a_function_call_without_a_preceding_assistant_still_produces_one() {
        // 模型偶尔 content 是空的，只有 tool_calls。历史里就没有那条
        // assistant_message，但还原时**必须造一条**装工具请求，
        // 否则 tool 消息前面没有对应的 assistant，请求直接 400。
        let items = vec![
            Item::user_message(1, "问题"),
            Item::function_call(2, 1, "c1", "search_videos", &json!({})),
            Item::function_call_output(3, 1, "c1", "结果", false, None),
        ];
        let msgs = to_messages(&items);

        assert_eq!(msgs.len(), 3);
        match &msgs[1] {
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                assert!(text.is_empty());
                assert_eq!(tool_calls.len(), 1);
            }
            other => panic!("必须补一条 assistant，实际 {other:?}"),
        }
    }

    #[test]
    fn error_results_come_back_marked_as_errors() {
        let items = vec![Item::function_call_output(
            1,
            1,
            "c1",
            "SC 返回 503",
            true,
            None,
        )];
        match &to_messages(&items)[0] {
            Msg::Tool(r) => assert!(r.is_error, "失败标记不能丢"),
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn an_empty_history_produces_no_messages() {
        assert!(to_messages(&[]).is_empty());
    }

    #[test]
    fn reasoning_is_restored_when_rebuilding_the_message_array() {
        // 存了但还原时丢掉，等于没存
        let items = vec![Item::assistant_message_full(1, 1, "我搜一下", "先搜关键词")];
        match &to_messages(&items)[0] {
            Msg::Assistant {
                text, reasoning, ..
            } => {
                assert_eq!(text, "我搜一下");
                assert_eq!(reasoning, "先搜关键词");
            }
            other => panic!("实际 {other:?}"),
        }
    }
}

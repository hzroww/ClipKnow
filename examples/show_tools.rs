//! 打印模型实际看到的工具清单（就是每轮请求里发出去的那份）。
use clipknow::agent::tools::tool_defs;
fn main() {
    for t in tool_defs() {
        println!("═══ {} ═══", t.name);
        let props = t.params["properties"].as_object().unwrap();
        let req: Vec<&str> = t.params["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for (k, v) in props {
            let ty = v["type"].as_str().unwrap_or("?");
            let en = v
                .get("enum")
                .map(|e| format!("  取值: {e}"))
                .unwrap_or_default();
            let d = v.get("description").and_then(|x| x.as_str()).unwrap_or("");
            println!(
                "  {k}: {ty}{}{}",
                if req.contains(&k.as_str()) {
                    " (必填)"
                } else {
                    " (可选)"
                },
                if d.is_empty() {
                    String::new()
                } else {
                    format!("  — {d}")
                }
            );
            if !en.is_empty() {
                println!("    {en}");
            }
        }
        println!("  说明: {}\n", t.description.replace('\n', "\n        "));
    }
    let total: usize = tool_defs()
        .iter()
        .map(|t| t.name.len() + t.description.chars().count() + t.params.to_string().len())
        .sum();
    println!("五个工具定义合计约 {} 字符（每轮请求都要带上）", total);
}

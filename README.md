# ClipKnow

分析社媒视频内容的命令行工具。丢一个视频链接进去，问它问题。

支持 YouTube / TikTok / Instagram。

## 准备

抓取的 key 是必需的，大模型的 key 两家有一个就行：

```bash
SCRAPECREATORS_API_KEY=...   # 必需，https://scrapecreators.com
DEEPSEEK_API_KEY=...         # https://platform.deepseek.com
ANTHROPIC_API_KEY=...        # https://console.anthropic.com/settings/keys
```

放进项目目录的 `.env`（已在 `.gitignore` 里）或 `~/.zshrc`。
两家都设了默认用 DeepSeek，`--provider anthropic` 可切回 Claude。

> 注意：Claude Code 的登录态不能当 `ANTHROPIC_API_KEY` 用，得去 Console 单独建一个。

### 两家的实测对比

同一个视频、同一个问题（57 秒的产品宣传片）：

| | 输入/输出 token | 成本 | 表现 |
|---|---|---|---|
| Claude Opus 5 | 970 / 471 | $0.0166 | 主动标注「这条来自简介而非文字稿」、指出转录把 Braun 识别成 brawn、说明画面信息材料里没有 |
| DeepSeek | 609 / 134 | $0.0003 | 答案准确干净，但未区分信息来自文字稿还是简介 |

**成本差 55 倍。** 日常开发调试用 DeepSeek 足够；需要严格溯源、不能编造的场景切回 Claude 对照。
第二步做 agent 循环时要重新评估——循环对「严格遵守指令」的要求比单次问答高得多。

Rust 工具链（本机已装）：

```bash
brew install rustup && rustup toolchain install stable
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
```

## 用法

```bash
cargo build

# 分析一个视频并提问
./target/debug/clipknow ask "https://www.youtube.com/watch?v=xxx" "这视频在讲什么？"

# 看库里存了这个视频的什么（调试时很常用）
./target/debug/clipknow show "https://www.youtube.com/watch?v=xxx"
./target/debug/clipknow show "https://..." --raw     # 连 SC 原始 JSON 一起看

# 列出抓过的视频
./target/debug/clipknow list
```

抓过的视频存在 `clipknow.db`（一个 SQLite 文件）里，再问同一个视频不会重复花钱。
加 `--refresh` 强制重抓。

## 跑测试

```bash
cargo test
```

## 代码结构

```
src/
├── main.rs                  CLI 入口
├── error.rs                 统一错误类型
├── ingest/                  ← 脏活隔离区，最容易因平台改动而坏
│   ├── url.rs                 链接 → (平台, 视频ID)，纯函数、不碰网络
│   └── scrapecreators.rs      调 SC，把三家不同的 JSON 翻译成统一类型
├── content/
│   ├── model.rs               Video / Transcript / Comment
│   └── evidence.rs            把材料拼成给模型看的文本
├── store/
│   ├── mod.rs                 Store trait
│   └── sqlite.rs              SQLite 实现
└── agent/
    └── llm.rs               ← 唯一知道「用哪家模型」的文件
```

两个隔离点值得注意：

- **`ingest/`**：SC 挂了或换供应商，只改这个目录。原始响应整个存进 `videos.raw_json`，
  解析漏了字段随时能补，不用重新花钱抓。
- **`agent/llm.rs`**：`ModelRequest` / `ModelResponse` 是自定义的中立类型，不是
  Anthropic 的结构。换成 DeepSeek 只改这一个文件，其余代码一行不动。

## 路线

- **第一步（当前）**：给链接、问问题，一次模型调用出答案。**没有 agent 循环**——
  单视频问答的证据在开始前就全定了，加循环是绕路。
- **第二步**：「帮我找几个做科普的博主」这类需求。要查哪几个频道取决于上一步搜到
  什么，没法提前写死，**这时循环才真正必要**。
- **第三步**：跨视频提问。先用 SQLite FTS5 关键词检索跑通，再考虑向量检索。

每一步为什么这么切，代码注释里都写了理由——尤其 `ingest/scrapecreators.rs`
和 `agent/llm.rs` 的文件头注释，记录了实测踩到的坑和当时的取舍。

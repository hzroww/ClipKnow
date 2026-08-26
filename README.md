# ClipKnow

分析社媒视频的命令行工具。两种用法:

- **给一条链接问问题** —— 「这视频在讲什么」
- **给一个开放式需求** —— 「帮我找几个做科普的 YouTube 博主」,它自己去搜、去查、去挑

支持 YouTube / TikTok / Instagram。

这是个学习项目,目的是把 agent 循环和 RAG 从零写一遍。所以刻意不用框架、不藏抽象——
每一层复杂度都由一个亲身撞到的问题触发,代码注释里记着当时的实测数据和取舍。

## 准备

```bash
SCRAPECREATORS_API_KEY=...   # 必需，https://scrapecreators.com
DEEPSEEK_API_KEY=...         # https://platform.deepseek.com
ANTHROPIC_API_KEY=...        # 可选，https://console.anthropic.com/settings/keys
```

放进项目目录的 `.env`(已在 `.gitignore` 里)或 `~/.zshrc`。默认用 DeepSeek。

> Claude Code 的登录态不能当 `ANTHROPIC_API_KEY` 用,得去 Console 单独建一个。

**两个 key 都是付费服务,先知道大概花多少:**

- **ScrapeCreators** 按次计费(1 次调用 = 1 credit)。实测一次「找博主」用
  **3–13 credit**,一次「某博主最近在发什么」用 **2–3 credit**。失败的调用不扣费。
- **DeepSeek** 按 token 计费,实测一次提问 **$0.003–0.017**(见下面「成本」一节)。

**Rust 工具链要 1.88 以上**(代码里用了 let-chains):

```bash
brew install rustup && rustup toolchain install stable
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
rustup update stable          # 半年前装的 stable 会编译失败
cargo build
```

## 用法

### 发现类需求(第二版)

```bash
# 交互模式：连续追问，↑ 调历史，/new 开新会话，/quit 退出
./target/debug/clipknow find

# 一次性
./target/debug/clipknow find "帮我找几个做科普的 YouTube 博主"

# 接着最近一次有活动的会话聊
./target/debug/clipknow find --continue

# 历史会话
./target/debug/clipknow sessions
```

模型手上有五个工具,自己决定调哪个、调几次:

| 工具 | YouTube | TikTok | Instagram |
|---|---|---|---|
| `search_videos` 按关键词搜视频 | ✅ | ✅ | ❌ 端点上游失效，见下 |
| `search_creators` 按关键词搜账号 | ✅ | ✅ | ✅ |
| `get_creator` 博主主页数据 | ✅ | ✅ | ✅ |
| `get_creator_videos` 博主近期视频 | ✅ | ✅ | ✅ |
| `fetch_video` 单条视频完整内容 | ✅ | ✅ | ✅ |

> **Instagram 的搜视频什么时候能恢复**：`/v2/instagram/reels/search` 现在对所有
> 查询词返回 404(2026-08-19 实测，含官方文档自己的示例 `dogs`)。如果你 clone 下来
> 时它已经修好了，把 `src/agent/tools.rs` 里 `SEARCHABLE_PLATFORMS` 加上
> `"instagram"`、`src/ingest/discovery.rs` 的 `route()` 里那条 `return None` 换成
> 真实路径即可，两行。

### 单视频问答(第一版)

```bash
./target/debug/clipknow ask "https://www.youtube.com/watch?v=xxx" "这视频在讲什么？"
./target/debug/clipknow show "https://..." --raw   # 看库里存了什么，含原始 JSON
./target/debug/clipknow list                        # 抓过的视频
```

抓过的视频存在 `clipknow.db`(一个 SQLite 文件),再问同一个视频不重复花钱。`--refresh` 强制重抓。

## 看它是怎么想的

循环跑完,决策路径全在库里,不是黑盒:

```bash
# 每一轮调了什么工具、传了什么参数
sqlite3 -header -column clipknow.db "
SELECT i.iteration AS 轮, json_extract(i.payload_json,'\$.name') AS 工具,
       json_extract(i.payload_json,'\$.args.query') AS 关键词,
       json_extract(i.payload_json,'\$.args.handle') AS 博主
FROM items i JOIN turns t ON t.id=i.turn_id
WHERE i.item_type='function_call' ORDER BY t.seq, i.idx"

# 实际花了多少 credit（不能数行数：失败的调用不扣费）
sqlite3 clipknow.db "SELECT SUM(json_extract(payload_json,'\$.credits_charged'))
                     FROM items WHERE item_type='function_call_output'"

# 配对自查：正常永远是空的，非空说明循环有 bug
sqlite3 clipknow.db "SELECT i.call_id FROM items i WHERE i.item_type='function_call'
  AND NOT EXISTS (SELECT 1 FROM items o WHERE o.turn_id=i.turn_id
    AND o.item_type='function_call_output' AND o.call_id=i.call_id)"
```

## 代码结构

```
src/
├── main.rs                  CLI：ask / find / sessions / show / list
├── error.rs                 统一错误类型
├── ingest/                  ← 脏活隔离区，最容易因平台改动而坏
│   ├── url.rs                 链接 → (平台, 视频ID)，纯函数、不碰网络
│   ├── scrapecreators.rs      调 SC，第一版那条抓取链路
│   └── discovery.rs           发现类端点的路由 + 三家响应 → 中立类型
├── content/
│   ├── model.rs               Video / Creator / VideoSummary / Item …
│   ├── evidence.rs            两套系统提示词 + 材料拼装（含注入防御）
│   └── dossier.rs             视觉档案：结构化字段 + 解析 + 渲染
├── store/
│   ├── mod.rs                 Store trait
│   └── sqlite.rs              SQLite 实现
└── agent/
    ├── llm.rs               ← 唯一知道「用哪家模型」的文件
    ├── tools.rs               五个工具的定义、参数校验、分发
    ├── runner.rs              agent 循环（状态机 + 三道闸门）
    ├── compaction.rs           历史太长时压成结构化摘要
    ├── vision.rs             ← 唯一知道「用哪家视觉模型」的文件
    ├── context.rs             库里的条目 ↔ 发给模型的消息数组
    └── fixtures/              13 份真实 SC 响应，单测用，不联网不花钱
```

三个隔离边界(都是 trait,换实现只改一个文件):

- **`LlmClient`** —— 换模型供应商。第二版加 DeepSeek 时只动了这一个文件。
- **`DiscoveryApi`** —— 换抓取供应商,也让循环能在不联网的情况下测试。
- **`Store`** —— 换存储。

## 数据库

```
sessions ──┬─ turns（一次提问）──┬─ items（这次提问里的所有条目）
           │   seq/model/status  │   idx / item_type / iteration
           │                     │   call_id / payload_json / raw_json
videos ────┴─ artifacts / transcripts / comments      ← 第一版，视频资料
```

**工具调用不单独建表**,它就是 `items` 里 `item_type` 不同的条目
(`user_message` / `assistant_message` / `function_call` / `function_call_output`)。
这样只有一套编号,消息和工具调用天然对得上。

两条持久化线,时机不同:

- **会话历史** → 终态一次性写,一个事务。半截落库会破坏 tool 配对不变量,
  下次 `--continue` 发出的请求必然 400
- **视频资料** → `fetch_video` 里立刻写。它是独立资料库,崩了留着是纯赚,
  还能让同会话内第二次 fetch 命中缓存

`payload_json` 存的是**当时实际发给模型的那段文本**,不是结构化数据——
存结构化的话重建历史时要重新渲染,而渲染代码一改,模型看到的「自己上一轮读过的材料」
就悄悄变了样。`raw_json` 是 SC 原始响应,**重建历史时不加载**(单条能有 2.4MB)。

## 循环的四道闸门

```
迭代上限      20 轮（第 16 轮插一句「该收敛了」，给模型预算感知）
SC 调用上限   25 次（按次计费，真金白银）
上下文预算    940,000 token（窗口 1M；基本不触发，防失控用）
视频分析上限  5 条（每条约 ¥0.07，一次提问上限约 ¥0.35）
```

视频分析必须**单独一道闸门**：一次分析在视觉模型那边是 2 万 token 起，远超任何
搜索结果，而它不是 SC 调用，`max_tool_calls` 数不到它。

两道闸门的**拦截位置不同**，这是实测调出来的：SC 那道在循环里，超了就不执行工具；
视频分析那道在工具内部，超了只跳过画面那一段——因为文字稿和评论本身有价值。
但有一种情况例外：带着 `question` 调 `fetch_video` 却没有视觉额度，那次调用是纯
浪费（`question` 的语义就是「我要看画面」），所以在花 SC 调用之前就拦住。

上下文判断用 provider 报回的**真实 `prompt_tokens`**,不是数字符——
ASCII 的 token 密度实测跨度 1.68～6.12 字符/token(句柄数字最密、英文散文最松),
没有单一比值能既准确又安全。字符估算只在两处兜底,取最保守的 1.68(宁可高估)。

**每次发请求之前都查一遍预算**,不只在开始时。原来漏了一条路:最后一个工具返回
20 万 token 之后,循环直接回到下一次模型调用,中间没有再查。

## 上下文压缩

历史攒到 40 万 token 就把最老的一段交给同一个模型摘要,压到 15 万,最近的原文保留。
静默进行,不打断提问。

**在 turn 外压,不在 turn 内。** 一个 turn = 一次提问 + 中间所有模型迭代 + 最终答案。
检查只在提问入口做一次,之后整个循环不再碰上下文。两个原因:

- 本轮自己产生的工具结果**压不动**——摘要掉一条 `function_call_output` 就破坏了
  tool_call/result 配对,下一次请求直接 400。循环里能做的只有中止,那是上面那道闸门的事。
- 检查点只有一个,「一次提问最多压一次」就是结构保证的。早先把检查放在循环每轮入口,
  而历史在循环里不变,于是每轮算出同一个切点、把同一段重复摘要——实跑一轮压了 3 次,
  只有最后一次有用。

切点只在**已完成 turn 的边界**上,从最老的往新扫,第一个「切完剩下的量 ≤ 目标」的边界
就选它,这样保留的近期原文最多。没有固定的「保留 N 轮」,完全由预算决定;最新那个 turn
的原文永远保留。

摘要是**结构化**的,字段按证据标准设计——`verified` 里每个候选带 handle、粉丝数和一行
`evidence`,要求数字原样搬过来。丢了数字的话,模型要么重新抓一遍(花钱),要么编一个
(直接违反证据标准)。已经有摘要时是**重新打包**:老摘要和这次要压的旧 turn 原文一起
喂进去,产出一个新摘要覆盖两者,不是两段摘要并列。

摘要落在 `turns.summary` / `turns.summarized_upto` 两列上,**`items` 一个字不改**——
压缩可回退(清空两列就恢复原状),存档保持忠实。生成失败就继续用当前上下文,不中止这一轮。

## 画面分析

`fetch_video` 返回**四段**材料：元数据 / 文字稿 / 评论 / **画面**。画面那一段是把
视频交给视觉模型（千问 `qwen3-vl-plus`）看完之后的结构化档案。

**为什么不做成单独的工具。** 试过分成 `fetch_video`（便宜）和 `analyze_video`
（贵）两个，问题是系统提示词里写着「每次工具调用都花钱，分层查」——一个被标成
「贵得多」的工具，模型几乎不会主动调，结果就是系统性地给出只看标题的片面回答。
而那是**设计造成的**。实测有的视频标题就是 `#tiktok #earth #Science`，一个字的
内容都没有；会调 `fetch_video` 这个动作本身就表示「我要深入了解这一条」。

### 视频怎么送到模型那边

三条路，实测只有一条对所有平台都成立：

| | 大小上限 | Instagram | 46MB 视频 |
|---|---|---|---|
| base64 内联进请求体 | **~20MB**（服务端 JSON 字符串上限 28,000,000 字符） | 小文件可以 | ❌ |
| 给平台 CDN 直链，让服务端自己拉 | 2GB | ❌ 拉不到 `cdninstagram` | ✅ |
| **上传到服务商的临时存储，给它一个引用** | 1GB | ✅ | ✅ |

所以走第三条：下载进内存 → 上传 → 拿一个可复用的引用。实测 6 条视频
（TikTok + Instagram，2.6MB–52.1MB，14.7s–487.4s）全部成功。

第二条那个 CDN 可达性差异值得记一下：同一个 TikTok 账号，`profile/videos` 端点
给的直链是 `*.tiktokcdn-eu.com`（服务端拉得到，7/7），`search` 端点给的是
`*.tiktokcdn-us.com`（0/9 全部超时），80 个样本零重叠。但那是供应商侧的行为，
会变——不建立在它上面。

### 换直链只打一个端点

CDN 直链 24–35 小时过期，上传引用 48 小时。所以会出现「引用死了、要重新下载」
而直链也已经过期的情况——这时候要换一个新直链。

直链只存在于**详情端点**的响应里（TikTok 的 `aweme_detail.video.play_addr`、
Instagram 的 `data.xdt_shortcode_media.video_url`），而文字稿和评论已经在库里。
所以只打详情那一个端点，`external_calls` 加 1 而不是 3。

原来这条路借的是「抓一条视频的完整内容」那把大锤，它内部打三个端点——三分之二
浪费，而且新抓的文字稿和评论还会把库里那份覆盖掉。这不是数据源的限制：三个端点
本来就独立，实测只打详情端点一次调用扣 1 credit，直链和过期时间都在。

### 三层复用

引用有 48 小时有效期，所以同一条视频的后续提问越来越便宜：

```
第一次看             下载 + 上传 + 分析     实测 42 秒
48 小时内带问题追问   只有分析              实测 7 秒
再问「这条讲什么」    纯读库                实测 0 秒
```

⚠️ **上传引用过期 ≠ 档案过期。** 档案永久有效，引用过期只影响「能不能带具体
问题追问」。参照的实现把这两件事混了，导致每 48 小时白白重新分析一遍——所以
列名叫 `staged_expires_at` 而不是 `expires_at`，并且有一条回归测试钉住。

### 抽帧率

```
fps = clamp(80 / 视频秒数, 0.1, 1.0)      带具体问题时下限抬到 0.5
```

目标是让**每条视频的 token 数大致恒定**，成本可预测。实测 96 秒和 487 秒的视频
都是 23,762 token（80 帧 × 约 297）。两个例外：短视频封在 1 fps 拿不到 80 帧
（30 秒就是 30 帧，无所谓，本来就便宜）；超过 800 秒的会突破目标，因为服务商的
fps 下限是 0.1——一条 1702 秒的视频实测 50,513 token，是目标的两倍。

### 档案字段

`summary` / `timeline` / `visible_text` / `spoken_content` / `entities` /
`limitations`。最后那格最容易被忽略但最重要：它写明这份档案的**分辨率**
（fps=0.2 就是每 5 秒才看一帧），不写清楚模型会以为档案覆盖了一切。

实测它还会引导追问——一条档案在 `limitations` 里写「地板上的白色线条是临时标记
还是投影效果无法确定」，追问之后重看给出了「疑似用胶带或颜料绘制，在 0:04–0:08
画面中」。

### 失败之后

视觉分析失败**绝不让整个工具失败**——文字材料是有效的。但失败要**记住**，而且
两类失败记的东西不同：

| | 记什么 | 下次会怎样 |
|---|---|---|
| **永久性**（内容审查、格式不对、太大） | 原因 | 直接返回原因，零下载零上传零分析 |
| **可重试**（限流、超时、5xx） | 上传引用 | 拿引用直接重试分析，不重新下载上传 |

起因是一条政治评论视频被内容审查拒了
（`DataInspectionFailed: Input video data may contain inappropriate content`）。
那个失败是**确定性的**——同一条视频每次都会被同样的审查拦下。而原来只在分析成功
时写档案，所以什么都没记住：下次再问又是一遍下载 + 上传 + 被拒。现在第一次 23 秒、
第二次 0 秒。

分类时**默认判为可重试**：可重试那条路会把上传引用留下来，重试只花一次分析调用；
误判成永久会让一条视频的画面永远看不了。代价不对称，所以往宽的一边错。

这和 SC 那边的 `is_transient()` 是同一个思路。

画面段的状态写明原因：
`未配置视觉模型` / `视频 340MB，超过上传上限` / `YouTube 暂不支持（拿不到直链）`
/ `本次提问的视频分析预算已用尽`。

和文字稿的 `[状态：没有文字稿]` 同一个道理：材料对某件事沉默时，模型只能靠
「我没看见」反推，而实测它会把推测说成材料里的标注。

## 一次提问的五种结局

```
Done              正常给出答案
Truncated         撞了 max_tokens，答案是残缺的 → 落库 failed
IterationCap      20 轮还没收敛
ContextBudget     历史太长，请求没发出去（不落库，因为什么都没发生）
ProtocolError     provider 返回了没见过的 stop_reason
ModelError        网络/API 报错
```

**`Truncated` 单独一种是有原因的**:原来只看「模型有没有要工具」来判断结束,
撞长度上限被砍掉半句话也会标成成功。然后 `--continue` 追问时历史里带着那半句,
模型以为自己上一轮就是这么说完的。

## 成本

`deepseek-v4-flash`,2026-08-19 官方价(高峰,低谷是一半):

| | 每百万 token |
|---|---|
| 输入(缓存未命中) | $0.44 |
| 输入(**命中前缀缓存**) | **$0.014** |
| 输出 | $1.32 |

**DeepSeek 自动做前缀缓存,不用声明任何东西。** 循环每轮重发历史,实测第二次请求
4094 个输入里 3968 命中(97%),这让循环的成本比预期低一个数量级。

实测:

| 场景 | 轮次 | 工具调用 | 输入 token | 成本 |
|---|---|---|---|---|
| 找科普博主 | 3–4 | 8–13 | 2.7–6.8 万 | $0.010–0.016 |
| 追问某个博主的更新频率 | 1 | **0**（复用历史） | 1.9 万 | $0.009 |
| 再追问（缓存热） | 1 | 0 | 2.9 万 | **$0.001** |
| 单视频问答（第一版） | 1 | — | 816 | $0.0003 |

**问题越具体越便宜**:「找几个科普博主」要 8 次工具调用,「毕导最近在发什么」只要 2 次。

**为什么加了证据标准之后变贵了**:模型现在每条推荐都要附一行「依据:我实际看到了
什么」,输出 token 从 ~1,200 涨到 ~3,500。答案从「听起来有道理」变成「每句话有出处」,
这个交换值得。

## 跑测试

```bash
cargo test                      # 225 个，不联网不花钱（夹具是真实响应）
cargo clippy --all-targets
```

`examples/` 下面三个是**真调 SC 的**,会花 credit:

```bash
cargo run --example probe_tools   # 五个工具各打一次 + fetch_video ×2 + 一次被拒 ≈ 8 credit
cargo run --example show_unified  # 只读夹具，不花钱
cargo run --example demo_rows     # 只读内存库，不花钱
```

## 路线

- **第一步(完成)**:给链接、问问题,一次模型调用出答案。**没有 agent 循环**——
  单视频问答的证据在开始前就全定了,加循环是绕路。
- **第二步(完成)**:「帮我找几个做科普的博主」。要查哪几个频道取决于上一步搜到什么,
  没法提前写死,**这时循环才真正必要**。
- **第三步**:跨视频提问。先用 SQLite FTS5 关键词检索跑通,再考虑向量检索——
  只有先体验过关键词在哪失灵,才能理解 embedding 在解决什么。

## 已知问题

- Instagram 的 `search_videos` 用不了:`/v2/instagram/reels/search` 对所有查询词返回
  404(含官方文档示例 `dogs`),`search/hashtag` 同样。其它三个工具正常,但
  Instagram 的发现质量会低于另外两家。
- 文字稿截断是**取头部**。一小时的访谈如果前 20 分钟在寒暄,砍前 4 万字正好全是废话。
  彻底的解法是分段摘要,要多次模型调用,推迟了。
- **终端可能吞字。** 实测过一次:打的是「帮我看看有什么美妆博主 tiktok上的」,
  程序收到的是「帮我看看有什么tiktok上的」——中间四个字在到达 stdin 之前就丢了
  (库里存着一条纯转义序列的「提问」作证:终端在往 stdin 注背景色查询应答和光标
  位置报告)。具体机制没查出来。缓解是两条:换了 `rustyline`(raw 模式下自己解析
  转义序列),以及**每次提问前回显收到的内容**:

  ```
  · 收到问题（15 字）：帮我看看有什么tiktok上的
  ```

  少了字一眼能看见,不用等答案出来发现答错了话题。

- 模型偶尔会在回答里汇报「材料里没有试图指挥我的内容」,而系统提示词明确说了
  没有就别提。改提示词措辞 + 回传思考过程之后两次没复现,但样本太小,还不确定。

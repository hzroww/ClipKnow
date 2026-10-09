package main

// 提问：转交给常驻的 Rust agent 服务，把它的 SSE 原样转发给浏览器。
//
// ## 改造前后
//
//	以前   每次提问 exec.Command 起一个 `clipknow turn` 子进程，
//	       读它 stdout 的 NDJSON，一行转一个 SSE 事件。答完进程死掉。
//	现在   POST 给常驻的 agent，它回一条 SSE 流，这里一行一行转发。
//
// 为什么要改：进程之间什么都不共享。「现在有哪几次提问在跑」「这个会话能不能
// 再接一个」「用户点了取消怎么通知」——这些状态在一个答完就死的进程里无处安放。
// 常驻之后它们有地方待了（Rust 那边的 app::registry）。
//
// ## 这个文件**不理解**那些 JSON 的内容
//
// 它不知道 tool_call 是什么意思，也不需要知道——翻译成人话在前端做。
// 好处是 Rust 那边加新事件类型时，这里一个字都不用改。
//
// 只有一处例外：判断流有没有正常收尾（endedProperly），那需要认识 done 和
// error 两个标签。那是**协议本身的约定**（Rust 永远以这两个之一结束，
// wire.rs 里有测试钉着），不是内容细节。
//
// ## 以前那把全局串行锁去哪了
//
// 删了。准入规则搬到 Rust 那边的 app::registry 了：同一个会话一次一个
// （409），全局最多 --max-turns 个（503）。搬过去是因为设计文档要求「只有
// 一个执行入口定义准入规则，网页和 CLI 不各写一套」——留在 Go 这边的话，
// 命令行绕过 web 直接跑就不受任何约束。

import (
	"bufio"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"strings"
	"time"
)

type chatReq struct {
	Session  string `json:"session"` // 空 = 新建一个会话
	Question string `json:"question"`
	Provider string `json:"provider"` // 空 = 让 Rust 自己挑
	// 浏览器点发送时生成的编号。自动重试带同一个，Rust 据此认出「同一次提交」：
	// 最多建一个会话、最多跑一轮。见 migrations/agent/009_chat_requests.sql。
	RequestID string `json:"request_id"`
}

// bufio.Scanner 单行上限。
//
// 默认是 64KB，而答案正文和工具调用参数都可能超过——超了 Scanner 会**静默
// 停止**（Scan 返回 false，Err() 是 ErrTooLong）。表现就是"答案没了"，
// 特别难查。放到 4MB，同时下面显式检查 Err()。
const maxLine = 4 << 20

func (s *Server) handleChat(w http.ResponseWriter, r *http.Request, u *Account) {
	if r.Method != http.MethodPost {
		http.Error(w, "只接受 POST", http.StatusMethodNotAllowed)
		return
	}
	var req chatReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "请求体不是合法 JSON", http.StatusBadRequest)
		return
	}
	if strings.TrimSpace(req.Question) == "" {
		http.Error(w, "问题是空的", http.StatusBadRequest)
		return
	}

	// SSE 需要能一段一段把数据推出去。拿不到 Flusher 说明中间隔了某种
	// 会缓冲整个响应的东西，那样流式就是假的——直接报错，别假装能用。
	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "这个环境不支持流式响应", http.StatusInternalServerError)
		return
	}

	requestID := strings.TrimSpace(req.RequestID)
	if requestID == "" {
		// 还没刷新的旧页面不带编号。自己生成一个：照常能问，只是这一次没法
		// 去重（浏览器重试时不会带同一个）。
		requestID = newRequestID()
	}

	// ★ 只调一次 Rust。新会话也是：会话由 Rust 在收到这次提交时建，和提交
	//   记录写在同一个事务里。以前这里先调一次「建会话」再调「提问」，重试
	//   时会多建一个会话（见 agentclient.go 的 StartTurn）。
	//
	// 归属校验、参数校验、去重、准入全在 agent 那一侧。这里只负责把它的拒绝
	// 翻译成浏览器该看到的状态码（见 writeAgentError 里的映射表）。
	//
	// ★ 注意这一步还**没有**给浏览器发任何响应头。所以拒绝能用 HTTP 状态
	//   码表达；一旦开始写流就只能用事件了。
	upstream, err := s.agent.StartTurn(u.ID, requestID, req.Session, req.Question, req.Provider)
	if err != nil {
		writeAgentError(w, err)
		return
	}
	defer upstream.Body.Close()

	if strings.HasPrefix(upstream.Header.Get("Content-Type"), "application/json") {
		// 同一次提交的重试：Rust 说「就是那一轮」。原样转给浏览器，前端看到
		// running 就去接着看，看到终态就去拉聊天记录。不重跑。
		w.Header().Set("Content-Type", "application/json; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		if _, err := io.Copy(w, io.LimitReader(upstream.Body, 64<<10)); err != nil {
			log.Printf("转发重复提交的响应失败: %v", err)
		}
		return
	}
	pipeSSE(w, flusher, upstream.Body)
}

// 浏览器没给编号时，Go 替它生成一个。32 位十六进制，满足 Rust 那边的格式。
func newRequestID() string {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		// crypto/rand 读失败在正常系统上不会发生；真发生了也只是这一次没法
		// 去重，用时间凑一个不重复的就行
		return fmt.Sprintf("go-%d", time.Now().UnixNano())
	}
	return hex.EncodeToString(b)
}

// GET /api/sessions/{id}/events —— 接着看一个正在跑的提问。
//
// 刷新页面、或者在另一个标签页打开同一个会话时，前端调这个。agent 那边
// 会先把这一轮已经发过的事件从第一条补发一遍，再接着实时推，直到结束。
//
// 没在跑（多半是刚答完）时 agent 回 404，原样转给前端；前端看到非 200
// 就去拉一遍聊天记录——答案那时已经在库里了。
func (s *Server) handleEvents(w http.ResponseWriter, r *http.Request, u *Account) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "这个环境不支持流式响应", http.StatusInternalServerError)
		return
	}
	upstream, err := s.agent.StreamEvents(u.ID, r.PathValue("id"))
	if err != nil {
		writeAgentError(w, err)
		return
	}
	defer upstream.Body.Close()
	pipeSSE(w, flusher, upstream.Body)
}

// POST /api/sessions/{id}/turns/{turn}/cancel —— 停止正在跑的那一轮。
//
// {turn} 是 hello 事件里的 turn 字段。按「这一轮」停而不是按会话停：
// 用户点了停止、请求在路上卡住，这期间那一轮自己答完了、用户又发了新问题——
// 按会话停的话，迟到的请求会把新问题停掉。
//
// 这里只转发。归属校验、编号核对、等它停下，全在 Rust 那一侧
// （src/serve/mod.rs 的 cancel_turn）。返回：
//
//	200 {"turn":…,"status":"cancelled"}   停好了，可以马上在这个会话里接着问
//	200 {"status":"done"}                 模型正在说的就是最终答案，让它说完了
//	202 {"status":"cancelling"}           还在等模型说完，接着看事件流
//	404                                   不是你的会话 / 没有这一轮
func (s *Server) handleCancel(w http.ResponseWriter, r *http.Request, u *Account) {
	code, res, err := s.agent.CancelTurn(u.ID, r.PathValue("id"), r.PathValue("turn"))
	if err != nil {
		writeAgentError(w, err)
		return
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(code)
	if err := json.NewEncoder(w).Encode(res); err != nil {
		log.Printf("写响应失败: %v", err)
	}
}

// 把 agent 的 SSE 一行一行原样转给浏览器。提问和「接着看」共用。
//
// 调用前**还没有**写任何响应头——拒绝要用 HTTP 状态码表达，只能在这之前。
func pipeSSE(w http.ResponseWriter, flusher http.Flusher, upstream io.Reader) {
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	// 有些反向代理会缓冲响应，那样 SSE 就废了。这个头是告诉 nginx 别缓冲。
	w.Header().Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	flusher.Flush()

	sc := bufio.NewScanner(upstream)
	sc.Buffer(make([]byte, 0, 64<<10), maxLine)

	// 最后一条 **data** 行。流结束后解析它一次，看是不是正常收尾。
	//
	// ★ 只记 data 行，不是"最后一行"：SSE 里还有心跳注释行（`:`，Rust 每
	//   15 秒发一个，防止反向代理掐掉空闲连接）和事件之间的空行。拿"最后
	//   一行"去判断的话，一个心跳正好落在最后就会被误判成异常退出。
	var lastData string

	for sc.Scan() {
		line := sc.Text()
		if payload, ok := strings.CutPrefix(line, "data: "); ok {
			lastData = payload
		}
		// 原样转发，包括心跳注释行——前端本来就只认 `data: ` 开头的行。
		if _, err := fmt.Fprintf(w, "%s\n", line); err != nil {
			// ★ 浏览器断了就直接走，**不再**替它把上游读完。
			//
			//   以前必须读完：事件是 Rust 边产生边塞进一个有界队列发给这一个
			//   连接的，这边不读，那边队列满了就开始丢进度。现在事件先记在
			//   Rust 那边的事件记录里（app::live），和有没有人在读完全无关——
			//   这边一关，Rust 那边负责这个连接的转发任务就退出了，agent 循环
			//   照跑、照落库，刷新后的页面还能从头补发。
			log.Printf("浏览器断开，agent 那边继续跑完（刷新后可以接着看）")
			return
		}
		flusher.Flush()
	}
	if err := sc.Err(); err != nil {
		log.Printf("读 agent 的流出错: %v", err)
		emitError(w, flusher, "读分析服务的输出出错: "+err.Error())
		return
	}

	// ★ 流半路断掉时**必须**告诉浏览器。
	//
	//   原来的行为实测过：接口返回 200 OK、响应体 0 字节，页面只是把进度
	//   收起来，什么都不说——「跑完了但没答案」和「崩了」长得一模一样。
	if !endedProperly(lastData) {
		emitError(w, flusher, "分析服务的响应中断了，这一轮没有正常结束")
	}
}

// 这条流是不是正常收尾的。
//
// Rust 那边**永远**以 done 或 error 结束（协议的一部分，wire.rs 里有测试
// 钉着）。所以只要最后一条 data 不是这两个之一——包括一条都没有——就说明
// 上游是半路断的。
//
// ⚠️ 这里破了一点点「Go 不理解 JSON 内容」的原则：它得认识这两个终止标签。
//
//	只破这一点点——**只解析最后一条**，不是每条都解析；而且「必须以终止
//	事件收尾」是协议本身的约定，不是内容细节。
func endedProperly(last string) bool {
	if last == "" {
		return false
	}
	var ev struct {
		T string `json:"t"`
	}
	if json.Unmarshal([]byte(last), &ev) != nil {
		return false
	}
	return ev.T == "done" || ev.T == "error"
}

// 兜底的错误事件。形状和 Rust 侧的 {"t":"error"} 一样，
// 这样前端只有一条处理路径。
func emitError(w http.ResponseWriter, f http.Flusher, msg string) {
	b, _ := json.Marshal(map[string]string{"t": "error", "message": msg})
	fmt.Fprintf(w, "data: %s\n\n", b)
	f.Flush()
}

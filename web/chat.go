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
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"strings"
)

type chatReq struct {
	Session  string `json:"session"` // 空 = 新建一个会话
	Question string `json:"question"`
	Provider string `json:"provider"` // 空 = 让 Rust 自己挑
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

	// ★ 新会话在**提问之前**建好，归属在创建那一刻写进库。
	//   改造前是 Rust 建完把 id 放在 hello 那一行报回来、Go 再认领一次——
	//   中间那一小段时间里会话是无主的，认领失败就永远无主了。
	//
	//   多一次本机 HTTP 往返，微秒级，换掉一个真实存在的竞态。
	session := req.Session
	if session == "" {
		id, err := s.agent.CreateSession(u.ID)
		if err != nil {
			writeAgentError(w, err)
			return
		}
		session = id
	}

	// 归属校验、参数校验、准入全在 agent 那一侧。这里只负责把它的拒绝
	// 翻译成浏览器该看到的状态码（见 writeAgentError 里的映射表）。
	//
	// ★ 注意这一步还**没有**给浏览器发任何响应头。所以拒绝能用 HTTP 状态
	//   码表达；一旦开始写流就只能用事件了。
	upstream, err := s.agent.StartTurn(u.ID, session, req.Question, req.Provider)
	if err != nil {
		writeAgentError(w, err)
		return
	}
	defer upstream.Body.Close()

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	// 有些反向代理会缓冲响应，那样 SSE 就废了。这个头是告诉 nginx 别缓冲。
	w.Header().Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	flusher.Flush()

	sc := bufio.NewScanner(upstream.Body)
	sc.Buffer(make([]byte, 0, 64<<10), maxLine)

	clientGone := false
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
		if clientGone {
			// ★ 浏览器断了也要**继续读**上游。
			//   不读的话 TCP 接收窗口很快满，Rust 那边的 SSE 写入被堵住，
			//   有界队列跟着满，进度事件开始被丢——那一轮还是会跑完落库，
			//   但白白丢了一堆进度。读完它成本几乎为零。
			continue
		}
		// 原样转发，包括心跳注释行——前端本来就只认 `data: ` 开头的行。
		if _, err := fmt.Fprintf(w, "%s\n", line); err != nil {
			clientGone = true
			log.Printf("浏览器断开，agent 那边继续跑完")
			continue
		}
		flusher.Flush()
	}
	if err := sc.Err(); err != nil {
		log.Printf("读 agent 的流出错: %v", err)
		if !clientGone {
			emitError(w, flusher, "读分析服务的输出出错: "+err.Error())
		}
		return
	}

	// ★ 流半路断掉时**必须**告诉浏览器。
	//
	//   原来的行为实测过：接口返回 200 OK、响应体 0 字节，页面只是把进度
	//   收起来，什么都不说——「跑完了但没答案」和「崩了」长得一模一样。
	if !clientGone && !endedProperly(lastData) {
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

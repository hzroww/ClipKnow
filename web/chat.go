package main

// 提问：拉起 Rust 子进程，把它 stdout 的每一行原样转成一个 SSE 事件。
//
// ★ 这个文件**不理解**那些 JSON 的内容。它不知道 tool_call 是什么意思，
//   也不需要知道——翻译成人话在前端做。好处是 Rust 那边加新事件类型时，
//   这里一个字都不用改。

import (
	"bufio"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os/exec"
	"sync"
	"time"
)

// 同一时刻只允许一个 turn 在跑。
//
// 硬约束是 SQLite 只允许一个写者：两个子进程同时写会互相等 5 秒然后失败。
// 另一个更要命的原因是**同一个会话并行会把历史搞乱**——两个进程读到同一份
// 历史，各自往后追加一个 turn，第二个的历史是过期的。
//
// 用 TryLock 而不是 Lock：抢不到就**立刻拒绝并说明原因**，不排队。排队的话
// 用户点了发送、界面一动不动，分不清是卡了还是坏了。
type turnGate struct {
	mu      sync.Mutex
	running bool
	since   time.Time
}

func (g *turnGate) acquire() bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.running {
		return false
	}
	g.running, g.since = true, time.Now()
	return true
}

func (g *turnGate) release() {
	g.mu.Lock()
	g.running = false
	g.mu.Unlock()
}

// 已经跑了多久。抢不到锁时拿它拼一句有信息量的话。
func (g *turnGate) elapsed() time.Duration {
	g.mu.Lock()
	defer g.mu.Unlock()
	if !g.running {
		return 0
	}
	return time.Since(g.since)
}

type chatReq struct {
	Session  string `json:"session"` // 空 = 让 Rust 新建一个
	Question string `json:"question"`
}

// bufio.Scanner 单行上限。
//
// 默认是 64KB，而答案正文和工具调用参数都可能超过——超了 Scanner 会**静默
// 停止**（Scan 返回 false，Err() 是 ErrTooLong）。表现就是"答案没了"，
// 特别难查。放到 4MB，同时下面显式检查 Err()。
const maxLine = 4 << 20

func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "只接受 POST", http.StatusMethodNotAllowed)
		return
	}
	var req chatReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "请求体不是合法 JSON", http.StatusBadRequest)
		return
	}
	if req.Question == "" {
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

	if !s.gate.acquire() {
		http.Error(w, fmt.Sprintf(
			"上一个问题还在跑（已经 %.0f 秒），等它结束再问",
			s.gate.elapsed().Seconds(),
		), http.StatusConflict)
		return
	}
	defer s.gate.release()

	args := []string{"--db", s.dbPath, "turn"}
	if req.Session != "" {
		args = append(args, "--session", req.Session)
	}
	args = append(args, req.Question)

	// ★ 刻意用 Command 而不是 CommandContext：**浏览器关掉不杀子进程**。
	//   那一轮已经花了 SC 调用配额、模型 token、可能还有一次视频分析的钱，
	//   为了"你关了页面"把这些扔掉是最亏的。它会照常跑完、照常写库，
	//   下次打开那个会话就能看到答案（只是中间的进度看不到了）。
	cmd := exec.Command(s.binPath, args...)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		http.Error(w, "起不了子进程: "+err.Error(), http.StatusInternalServerError)
		return
	}
	// Rust 那边人看的日志走 stderr（抓取重试之类）。转到服务端日志里，
	// 出问题时有东西可查。
	stderr, _ := cmd.StderrPipe()

	if err := cmd.Start(); err != nil {
		http.Error(w, "起不了子进程: "+err.Error(), http.StatusInternalServerError)
		return
	}
	if stderr != nil {
		go func() {
			sc := bufio.NewScanner(stderr)
			for sc.Scan() {
				log.Printf("[rust] %s", sc.Text())
			}
		}()
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	// 有些反向代理会缓冲响应，那样 SSE 就废了。这个头是告诉 nginx 别缓冲。
	w.Header().Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	flusher.Flush()

	sc := bufio.NewScanner(stdout)
	sc.Buffer(make([]byte, 0, 64<<10), maxLine)

	clientGone := false
	for sc.Scan() {
		line := sc.Bytes()
		if clientGone {
			// ★ 浏览器断了也要**继续读**子进程的 stdout。
			//   不读的话管道很快写满，子进程就卡在 write 上再也动不了——
			//   那才是真的把这一轮弄丢了。
			continue
		}
		// SSE 的格式：data: <一行>，空行结束一个事件。
		// Rust 那边保证一个事件正好一行（有测试钉着），所以这里直接拼。
		if _, err := fmt.Fprintf(w, "data: %s\n\n", line); err != nil {
			clientGone = true
			log.Printf("浏览器断开，子进程继续跑完")
			continue
		}
		flusher.Flush()
	}
	if err := sc.Err(); err != nil {
		log.Printf("读子进程输出出错: %v", err)
		if !clientGone {
			emitError(w, flusher, "读子进程输出出错: "+err.Error())
		}
	}

	if err := cmd.Wait(); err != nil && !clientGone {
		// Rust 那边出错时已经自己推了一条 {"t":"error"}，这里只补一句
		// 兜底——比如它是被信号杀掉的，那种情况什么都没推出来。
		log.Printf("子进程退出: %v", err)
	}
}

// 兜底的错误事件。形状和 Rust 侧的 {"t":"error"} 一样，
// 这样前端只有一条处理路径。
func emitError(w http.ResponseWriter, f http.Flusher, msg string) {
	b, _ := json.Marshal(map[string]string{"t": "error", "message": msg})
	fmt.Fprintf(w, "data: %s\n\n", b)
	f.Flush()
}

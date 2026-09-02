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
	"path/filepath"
	"strings"
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
	Provider string `json:"provider"` // 空 = 让 Rust 自己挑
}

// 允许的 provider。
//
// ★ 这个值来自浏览器，是外部输入。必须走白名单——虽然 exec.Command 不经过
//
//	shell（不存在命令注入），但一个任意字符串会被原样递给 clap，
//	比如传个 "--db" 进来就成了参数注入。
var okProvider = map[string]bool{"deepseek": true, "anthropic": true}

// bufio.Scanner 单行上限。
//
// 默认是 64KB，而答案正文和工具调用参数都可能超过——超了 Scanner 会**静默
// 停止**（Scan 返回 false，Err() 是 ErrTooLong）。表现就是"答案没了"，
// 特别难查。放到 4MB，同时下面显式检查 Err()。
const maxLine = 4 << 20

func (s *Server) handleChat(w http.ResponseWriter, r *http.Request, code string, u *User) {
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
	// 接着别人的会话问，等于能看别人的历史。列表过滤挡不住直接拼 id。
	if req.Session != "" && !s.access.CanSee(req.Session, code, u) {
		http.Error(w, "这不是你的会话", http.StatusForbidden)
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

	// ★ 在**起子进程之前**扣。一次提问一旦开始就已经在花钱了（SC 调用、
	//   模型 token、可能还有视频分析），哪怕最后失败。按「开始」计费才对得上
	//   实际支出。放在拿锁之后，是为了让「排队被拒」不白扣一次。
	allowed, left := s.access.Spend(code)
	if !allowed {
		http.Error(w, "你的提问次数用完了", http.StatusPaymentRequired)
		return
	}
	if left != unlimited {
		log.Printf("[%s] 提问，剩 %d 次", u.Name, left)
	}

	args := []string{"--db", s.dbPath}
	if req.Provider != "" {
		if !okProvider[req.Provider] {
			http.Error(w, "不认识的模型: "+req.Provider, http.StatusBadRequest)
			return
		}
		args = append(args, "--provider", req.Provider)
	}
	args = append(args, "turn")
	if req.Session != "" {
		args = append(args, "--session", req.Session)
	}
	args = append(args, req.Question)

	// ★ 刻意用 Command 而不是 CommandContext：**浏览器关掉不杀子进程**。
	//   那一轮已经花了 SC 调用配额、模型 token、可能还有一次视频分析的钱，
	//   为了"你关了页面"把这些扔掉是最亏的。它会照常跑完、照常写库，
	//   下次打开那个会话就能看到答案（只是中间的进度看不到了）。
	cmd := exec.Command(s.binPath, args...)
	// ★ 显式钉住子进程的工作目录，不继承服务自己的。
	//
	//   Rust 那边靠 dotenvy 从当前目录**往上级找** .env（DeepSeek 的 key
	//   在那里面，~/.zshrc 里只有 SC 和千问的）。继承 cwd 的话，服务从哪个
	//   目录启动就决定了子进程能不能找到 key——这本身就不该是个变量。
	//
	//   实际踩过：提交代码时 `git stash -u` 把当时还未跟踪的 web/ 整个删掉，
	//   pop 之后建回来的是**新 inode**，而运行中的服务的 cwd 还指着那个已被
	//   删除的旧目录。从一个不存在的目录往上找不到任何东西，于是第三个问题
	//   突然报「缺少环境变量」，而前两个好好的。
	cmd.Dir = filepath.Dir(s.dbPath)
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
	// Rust 那边人看的日志走 stderr。转到服务端日志的同时**留最后几行在内存里**：
	// 子进程要是没来得及吐任何 JSON 就死了（被 kill、panic），那几行就是浏览器上
	// 唯一能说清原因的东西。
	var tailMu sync.Mutex
	var tail []string
	if stderr != nil {
		go func() {
			sc := bufio.NewScanner(stderr)
			for sc.Scan() {
				l := sc.Text()
				log.Printf("[rust] %s", l)
				if strings.TrimSpace(l) == "" {
					continue
				}
				tailMu.Lock()
				tail = append(tail, l)
				if len(tail) > stderrTailLines {
					tail = tail[len(tail)-stderrTailLines:]
				}
				tailMu.Unlock()
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
	claimed := false
	// 最后一行原样留着。流结束后解析它一次，看是不是正常收尾——
	// 见下面 endedProperly 的注释。
	var lastLine []byte
	for sc.Scan() {
		line := sc.Bytes()
		lastLine = append(lastLine[:0], line...) // Scan 会复用底层数组，必须拷一份
		if clientGone {
			// ★ 浏览器断了也要**继续读**子进程的 stdout。
			//   不读的话管道很快写满，子进程就卡在 write 上再也动不了——
			//   那才是真的把这一轮弄丢了。
			continue
		}
		// hello 那一行带着会话 id（新会话是 Rust 建的）。记一笔归属，
		// 别人就看不到这个会话了。**只解析 hello 这一行**，其余照样原样转发。
		if !claimed {
			if id := helloSession(line); id != "" {
				s.access.Claim(id, code)
				claimed = true
			}
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

	err = cmd.Wait()
	if err != nil {
		log.Printf("子进程退出: %v", err)
	}

	// ★ 子进程半路死掉时**必须**告诉浏览器。
	//
	//   实测过原来的行为：拿一个必定失败的子进程去跑，接口返回 200 OK、
	//   响应体 0 字节，页面只是把进度收起来，什么都不说——「跑完了但没答案」
	//   和「崩了」长得一模一样。失败信息全被吞进了服务端日志。
	//   （原来这里只有一句 log.Printf，而注释却写着「补一句兜底」，
	//     描述了一件代码根本没做的事。）
	if !clientGone && !endedProperly(lastLine) {
		tailMu.Lock()
		why := strings.Join(tail, "\n")
		tailMu.Unlock()
		msg := "分析进程异常退出"
		if err != nil {
			msg += "（" + err.Error() + "）"
		}
		if why != "" {
			// panic 的堆栈就在这几行里，比干巴巴一句「异常退出」有用得多
			msg += "\n" + why
		}
		emitError(w, flusher, msg)
	}
}

// 服务端保留的 stderr 行数。够放下一条 panic 的头几行，又不至于把一次长跑的
// 抓取重试日志全堆在内存里。
const stderrTailLines = 8

// 这条流是不是正常收尾的。
//
// Rust 那边**永远**以 done 或 error 结束（协议的一部分，wire.rs 里有测试
// 钉着）。所以只要最后一行不是这两个之一——包括一行都没有——就说明子进程
// 是半路死的。
//
// ⚠️ 这里破了一点点「Go 不理解 JSON 内容」的原则：它得认识这两个终止标签。
//
//	只破这一点点——**只解析最后一行**，不是每行都解析；而且「必须以终止
//	事件收尾」是协议本身的约定，不是内容细节。
//	换成「退出码非 0 就报错」不行：Rust 自己报过 error 之后照样退出 1
//	（手敲命令时要有退出码可看），那样会重复报两遍。
func endedProperly(last []byte) bool {
	if len(last) == 0 {
		return false
	}
	var ev struct {
		T string `json:"t"`
	}
	if json.Unmarshal(last, &ev) != nil {
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

// hello 那一行里的会话 id。不是 hello 就返回空串。
//
// 这是 Go 第二处（也是最后一处）需要看懂 JSON 内容的地方——另一处是判断
// 流有没有正常收尾。两处都只认协议里最稳定的那点东西：事件类型标签。
func helloSession(line []byte) string {
	var ev struct {
		T       string `json:"t"`
		Session string `json:"session"`
	}
	if json.Unmarshal(line, &ev) != nil || ev.T != "hello" {
		return ""
	}
	return ev.Session
}

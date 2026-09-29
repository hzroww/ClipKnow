package main

// ClipKnow 的 web 服务。
//
// 它只做三件事：发页面、只读地查会话、把提问转给 Rust 子进程。
// 真正的分析（工具循环、压缩、预算闸门、视频下载上传分析）全在 Rust 那边，
// 这里一点都不碰。
//
// 进程模型（过渡期，下一个提交收尾）：
//   查会话     这一个进程 + 常驻的 clipknow serve（HTTP）
//   提问       这一个 + 一个临时的 clipknow turn 子进程（答完就退）
//
// 提问那条路还没搬到 HTTP 上，所以现在两条路并存。

import (
	"embed"
	"encoding/json"
	"flag"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
)

//go:embed static
var staticFS embed.FS

type Server struct {
	// ⚠️ 过渡期残留：只用来做提问前的归属检查。提问改走 HTTP 之后
	//    （下一个提交）整个删掉，见 store.go 顶部。
	store   *Store
	dbPath  string
	binPath string
	gate    turnGate

	// 会话的增删查改全问它。Go 不再直接读 sessions / turns / items。
	agent *AgentClient

	accounts *Accounts     // 账号与登录态（Go 拥有，Rust 不碰）
	limiter  *loginLimiter // 登录/注册的入口限速
}

func main() {
	// 默认路径按「在 web/ 目录里 go run .」算。
	addr := flag.String("addr", ":3000", "监听地址")
	db := flag.String("db", "../clipknow.db", "数据库文件")
	bin := flag.String("bin", "../target/release/clipknow", "clipknow 可执行文件")
	agentAddr := flag.String("agent", "http://127.0.0.1:3100",
		"常驻 agent 服务的地址（clipknow serve 监听的那个）")
	importAcc := flag.Bool("import-accounts", false,
		"一次性：把 access.json 里的邀请码变成真账号，打印初始密码后退出")
	flag.Parse()

	dbAbs, err := filepath.Abs(*db)
	if err != nil {
		log.Fatalf("数据库路径不对: %v", err)
	}
	// ★ 一次性的导入动作，放在「检查 Rust 二进制」之前——它只碰账号表，
	//   不需要 Rust。放在后面的话，没编译过 Rust 的机器上导入不了账号。
	if *importAcc {
		if err := importAccounts(defaultAccessPath(dbAbs), dbAbs); err != nil {
			log.Fatalf("导入失败: %v", err)
		}
		return
	}

	binAbs, err := filepath.Abs(*bin)
	if err != nil {
		log.Fatalf("可执行文件路径不对: %v", err)
	}
	// 在启动时就检查，而不是等第一次提问才发现——那时用户已经等了半天，
	// 还以为是模型慢
	if _, err := os.Stat(binAbs); err != nil {
		log.Fatalf("找不到 clipknow：%s\n先跑一次 cargo build --release", binAbs)
	}

	st, err := OpenStore(dbAbs)
	if err != nil {
		log.Fatalf("%v\n库还不存在的话，先用命令行问一次把它建出来", err)
	}
	defer st.Close()

	acc, err := OpenAccounts(dbAbs)
	if err != nil {
		log.Fatalf("%v", err)
	}
	defer acc.Close()

	// ★ 密钥没配就直接起不来，不存在「没配就不校验」的降级路径。
	signer, err := newTokenSigner()
	if err != nil {
		log.Fatalf("%v", err)
	}
	agent, err := NewAgentClient(*agentAddr, signer)
	if err != nil {
		log.Fatalf("%v", err)
	}
	// ★ 启动时就探一次，而不是等第一次请求。
	//   配错地址、agent 没起、两边密钥不一致——这三样在这里报出来，
	//   人还在终端前面；等到用户提问时才报，错误信息还会指向错误的方向
	//   （401 在浏览器上长得像「登录过期」）。
	if err := agent.Probe(); err != nil {
		log.Fatalf("%v", err)
	}

	s := &Server{
		store: st, dbPath: dbAbs, binPath: binAbs,
		agent:    agent,
		accounts: acc, limiter: newLoginLimiter(),
	}

	mux := s.routes()

	log.Printf("ClipKnow web  →  http://localhost%s", *addr)
	log.Printf("  库   %s", dbAbs)
	log.Printf("  程序 %s", binAbs)
	if err := http.ListenAndServe(*addr, mux); err != nil {
		log.Fatal(err)
	}
}

// 路由表。
//
// 抽成函数是为了让端到端测试能起一个一模一样的服务（httptest）。
// 测试里自己再列一遍的话，这里加了接口那边忘了加，测试就会悄悄漏掉它。
func (s *Server) routes() *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("/api/register", s.handleRegister)
	mux.HandleFunc("/api/auth/login", s.handleLoginNew)
	mux.HandleFunc("/api/auth/logout", s.handleLogoutNew)
	mux.HandleFunc("/api/me", s.handleMe)
	mux.HandleFunc("/api/sessions", s.needAuth(s.handleSessions))
	mux.HandleFunc("/api/sessions/", s.needAuth(s.handleHistory))
	mux.HandleFunc("/api/chat", s.needAuth(s.handleChat))
	// 登录页本身不设防——它就是那个输用户名密码的地方
	mux.HandleFunc("/", s.handleIndex)
	return mux
}

func (s *Server) handleIndex(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	b, err := staticFS.ReadFile("static/index.html")
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Write(b)
}

func (s *Server) handleSessions(w http.ResponseWriter, r *http.Request, u *Account) {
	// 归属过滤发生在 Rust 那一侧的 SQL WHERE 里。Go 连表名都不需要知道。
	list, err := s.agent.Sessions(u.ID, 200)
	if err != nil {
		writeAgentError(w, err)
		return
	}
	// 给浏览器的仍然是裸数组，前端一个字都不用改。
	// （Rust 那边外面套了一层 {"sessions":[...]}，是为了以后能加分页游标。）
	writeJSON(w, list)
}

// GET /api/sessions/<id>
func (s *Server) handleHistory(w http.ResponseWriter, r *http.Request, u *Account) {
	id := strings.TrimPrefix(r.URL.Path, "/api/sessions/")
	if id == "" || strings.Contains(id, "/") {
		http.Error(w, "会话 id 不对", http.StatusBadRequest)
		return
	}
	// ★ 归属检查在 Rust 那一侧。不是自己的会话返回 **404**，和「不存在」
	//   一个表现——回 403 等于告诉对方「有这么个会话，只是不给你看」，
	//   那本身就是信息泄漏。
	//
	//   （改造前这里返回的是空列表 + 200，界面上显示成「这个会话是空的」。
	//     同样不泄漏内容，但对方由此知道这个 id 确实存在。现在统一成 404。）
	msgs, err := s.agent.Messages(u.ID, id)
	if err != nil {
		writeAgentError(w, err)
		return
	}
	writeJSON(w, msgs)
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	if err := json.NewEncoder(w).Encode(v); err != nil {
		log.Printf("写响应失败: %v", err)
	}
}

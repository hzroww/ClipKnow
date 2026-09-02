package main

// ClipKnow 的 web 服务。
//
// 它只做三件事：发页面、只读地查会话、把提问转给 Rust 子进程。
// 真正的分析（工具循环、压缩、预算闸门、视频下载上传分析）全在 Rust 那边，
// 这里一点都不碰。
//
// 进程模型：
//   空闲时      这一个进程
//   提问期间    这一个 + 一个临时的 clipknow turn 子进程（答完就退）

import (
	"embed"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
)

//go:embed static
var staticFS embed.FS

type Server struct {
	store   *Store
	access  *Access
	dbPath  string
	binPath string
	gate    turnGate
}

func main() {
	// 默认路径按「在 web/ 目录里 go run .」算。
	addr := flag.String("addr", ":3000", "监听地址")
	db := flag.String("db", "../clipknow.db", "数据库文件")
	bin := flag.String("bin", "../target/release/clipknow", "clipknow 可执行文件")
	invite := flag.String("invite", "", "生成一个邀请码给这个人，然后退出。用法：-invite 张三")
	quota := flag.Int("quota", 5, "配合 -invite：这个人能问几次")
	flag.Parse()

	dbAbs, err := filepath.Abs(*db)
	if err != nil {
		log.Fatalf("数据库路径不对: %v", err)
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

	ac, err := LoadAccess(defaultAccessPath(dbAbs))
	if err != nil {
		log.Fatalf("读不了邀请码文件: %v", err)
	}

	// -invite 是个一次性动作：生成码、打出来、退出。不起服务。
	if *invite != "" {
		code, err := ac.Invite(*invite, *quota)
		if err != nil {
			log.Fatalf("写不了邀请码文件: %v", err)
		}
		fmt.Printf("\n给「%s」的邀请码：%s（%d 次提问）\n\n", *invite, code, *quota)
		return
	}

	st, err := OpenStore(dbAbs)
	if err != nil {
		log.Fatalf("%v\n库还不存在的话，先用命令行问一次把它建出来", err)
	}
	defer st.Close()

	s := &Server{store: st, access: ac, dbPath: dbAbs, binPath: binAbs}

	mux := http.NewServeMux()
	mux.HandleFunc("/api/login", s.handleLogin)
	mux.HandleFunc("/api/logout", s.handleLogout)
	mux.HandleFunc("/api/me", s.handleMe)
	mux.HandleFunc("/api/sessions", s.needAuth(s.handleSessions))
	mux.HandleFunc("/api/sessions/", s.needAuth(s.handleHistory))
	mux.HandleFunc("/api/chat", s.needAuth(s.handleChat))
	// 登录页本身不设防——它就是那个输邀请码的地方
	mux.HandleFunc("/", s.handleIndex)

	log.Printf("ClipKnow web  →  http://localhost%s", *addr)
	log.Printf("  库   %s", dbAbs)
	log.Printf("  程序 %s", binAbs)
	if err := http.ListenAndServe(*addr, mux); err != nil {
		log.Fatal(err)
	}
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

func (s *Server) handleSessions(w http.ResponseWriter, r *http.Request, code string, u *User) {
	list, err := s.store.Sessions(200)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	// 只给他自己的。管理员看得见全部，包括命令行建的那些
	// （那些在 Owner 表里没有记录，正是你自己的历史）。
	mine := []Session{}
	for _, v := range list {
		if s.access.CanSee(v.ID, code, u) {
			mine = append(mine, v)
		}
	}
	writeJSON(w, mine)
}

// GET /api/sessions/<id>
func (s *Server) handleHistory(w http.ResponseWriter, r *http.Request, code string, u *User) {
	id := strings.TrimPrefix(r.URL.Path, "/api/sessions/")
	if id == "" || strings.Contains(id, "/") {
		http.Error(w, "会话 id 不对", http.StatusBadRequest)
		return
	}
	// ★ 必须在这里查一次，不能只靠列表过滤：会话 id 是可以直接拼进 URL 的，
	//   光在列表里藏起来不算隔离。
	if !s.access.CanSee(id, code, u) {
		http.Error(w, "这不是你的会话", http.StatusForbidden)
		return
	}
	msgs, err := s.store.History(id)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
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

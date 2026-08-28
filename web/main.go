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
	dbPath  string
	binPath string
	gate    turnGate
}

func main() {
	// 默认路径按「在 web/ 目录里 go run .」算。
	addr := flag.String("addr", ":3000", "监听地址")
	db := flag.String("db", "../clipknow.db", "数据库文件")
	bin := flag.String("bin", "../target/release/clipknow", "clipknow 可执行文件")
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

	st, err := OpenStore(dbAbs)
	if err != nil {
		log.Fatalf("%v\n库还不存在的话，先用命令行问一次把它建出来", err)
	}
	defer st.Close()

	s := &Server{store: st, dbPath: dbAbs, binPath: binAbs}

	mux := http.NewServeMux()
	mux.HandleFunc("/api/sessions", s.handleSessions)
	mux.HandleFunc("/api/sessions/", s.handleHistory)
	mux.HandleFunc("/api/chat", s.handleChat)
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

func (s *Server) handleSessions(w http.ResponseWriter, r *http.Request) {
	list, err := s.store.Sessions(100)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	writeJSON(w, list)
}

// GET /api/sessions/<id>
func (s *Server) handleHistory(w http.ResponseWriter, r *http.Request) {
	id := strings.TrimPrefix(r.URL.Path, "/api/sessions/")
	if id == "" || strings.Contains(id, "/") {
		http.Error(w, "会话 id 不对", http.StatusBadRequest)
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

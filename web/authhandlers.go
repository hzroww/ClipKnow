package main

// 注册 / 登录 / 退出 / 我是谁。

import (
	"encoding/json"
	"errors"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"
)

// 登录尝试限速。
//
// 挡的是「拿常见密码轮着试」。不是给用户设使用额度——设计文档明确区分了这两件
// 事：容量保护不等于付费配额。
//
// 按 IP 计数而不是按用户名：按用户名的话，攻击者拿一个密码去撞一万个用户名，
// 每个用户名只试一次，永远触发不了限制（这叫 password spraying）。
type loginLimiter struct {
	mu   sync.Mutex
	hits map[string][]time.Time
}

const (
	loginWindow   = 15 * time.Minute
	loginMaxTries = 20
	// 失败时故意慢一点。把「每秒几千次」变成「每秒几次」，几乎不要钱。
	loginFailDelayNew = 300 * time.Millisecond
)

func newLoginLimiter() *loginLimiter {
	return &loginLimiter{hits: map[string][]time.Time{}}
}

// 记一次尝试，返回是否还允许。
func (l *loginLimiter) allow(key string) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := time.Now()
	cutoff := now.Add(-loginWindow)
	kept := l.hits[key][:0]
	for _, t := range l.hits[key] {
		if t.After(cutoff) {
			kept = append(kept, t)
		}
	}
	// ★ 顺手清掉已经空了的键，不然这张 map 会随着 IP 数量无限涨。
	if len(kept) == 0 {
		delete(l.hits, key)
	} else {
		l.hits[key] = kept
	}
	if len(kept) >= loginMaxTries {
		return false
	}
	l.hits[key] = append(l.hits[key], now)
	return true
}

func clientIP(r *http.Request) string {
	if f := r.Header.Get("X-Forwarded-For"); f != "" {
		// 取第一个，那是最靠近客户端的那一跳
		if i := strings.IndexByte(f, ','); i >= 0 {
			f = f[:i]
		}
		return strings.TrimSpace(f)
	}
	// ★ 必须去掉端口。
	//
	//   RemoteAddr 是 "127.0.0.1:52341" 这种形式，**端口每条新连接都不一样**。
	//   直接拿它当键的话，每次请求都算一个新客户端，限速完全不生效。
	//   实测：全量跑测试时试满 25 次也没被拦住（单独跑碰巧因为连接复用没暴露）。
	if host, _, err := net.SplitHostPort(r.RemoteAddr); err == nil {
		return host
	}
	return r.RemoteAddr
}

type credReq struct {
	Username string `json:"username"`
	Password string `json:"password"`
}

// 统一的错误响应。code 给前端做判断，message 给人看。
func writeErr(w http.ResponseWriter, status int, code, msg string) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"code": code, "message": msg})
}

// 只接受 POST + 同源。修改状态的接口都要走这一遍。
func (s *Server) guardPost(w http.ResponseWriter, r *http.Request) bool {
	if r.Method != http.MethodPost {
		writeErr(w, http.StatusMethodNotAllowed, "method_not_allowed", "只接受 POST")
		return false
	}
	if !sameOrigin(r) {
		writeErr(w, http.StatusForbidden, "cross_origin", "跨站请求被拒绝")
		return false
	}
	return true
}

func (s *Server) handleRegister(w http.ResponseWriter, r *http.Request) {
	if !s.guardPost(w, r) {
		return
	}
	if !s.limiter.allow(clientIP(r)) {
		writeErr(w, http.StatusTooManyRequests, "rate_limited", "太频繁了，等一会儿再试")
		return
	}
	var req credReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "bad_json", "请求体不是合法 JSON")
		return
	}
	u, err := s.accounts.Create(req.Username, req.Password, false)
	switch {
	case errors.Is(err, ErrUsernameTaken):
		writeErr(w, http.StatusConflict, "username_taken", ErrUsernameTaken.Error())
		return
	case errors.Is(err, ErrBadUsername):
		writeErr(w, http.StatusBadRequest, "bad_username", ErrBadUsername.Error())
		return
	case errors.Is(err, ErrBadPassword):
		writeErr(w, http.StatusBadRequest, "bad_password", ErrBadPassword.Error())
		return
	case err != nil:
		writeErr(w, http.StatusInternalServerError, "internal", "注册失败")
		return
	}
	// 注册完直接登录，省一步
	s.issueSession(w, r, u)
}

func (s *Server) handleLoginNew(w http.ResponseWriter, r *http.Request) {
	if !s.guardPost(w, r) {
		return
	}
	if !s.limiter.allow(clientIP(r)) {
		writeErr(w, http.StatusTooManyRequests, "rate_limited", "尝试太多了，等一会儿再试")
		return
	}
	var req credReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "bad_json", "请求体不是合法 JSON")
		return
	}
	u, err := s.accounts.Authenticate(req.Username, req.Password)
	if err != nil {
		time.Sleep(loginFailDelayNew)
		if errors.Is(err, ErrUserDisabled) {
			writeErr(w, http.StatusForbidden, "disabled", ErrUserDisabled.Error())
			return
		}
		// 用户名不存在和密码错是同一个回答——否则这个接口就成了
		// 「查这个用户名注册过没有」的工具
		writeErr(w, http.StatusUnauthorized, "bad_credentials", ErrNoSuchUser.Error())
		return
	}
	s.issueSession(w, r, u)
}

func (s *Server) issueSession(w http.ResponseWriter, r *http.Request, u *Account) {
	token, err := s.accounts.NewSession(u.ID)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, "internal", "建登录态失败")
		return
	}
	setAuthCookie(w, r, token)
	writeJSON(w, u)
}

func (s *Server) handleLogoutNew(w http.ResponseWriter, r *http.Request) {
	// 退出不查同源：万一 Cookie 真被别的站点利用了，让它「被登出」也是安全的
	if tok := authTokenFrom(r); tok != "" {
		_ = s.accounts.RevokeSession(tok)
	}
	clearAuthCookie(w)
	w.WriteHeader(http.StatusNoContent)
}

// 从请求认人。认不出返回 nil。
func (s *Server) accountFrom(r *http.Request) *Account {
	u, err := s.accounts.BySessionToken(authTokenFrom(r))
	if err != nil {
		return nil
	}
	return u
}

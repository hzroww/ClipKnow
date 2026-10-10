package main

// 长期记忆面板：列出、删除。
//
// 记忆存在 Rust 那边的 memories 表里，Go 一行 SQL 都没有（和聊天表一样，
// agentclient_test.go 里的守卫测试钉着这件事）。这里只做两件事：认人、转发。

import "net/http"

// GET /api/memories
//
// 返回 {"memories":[...],"limits":{"profile":3,...}}，形状和 Rust 那边一样。
func (s *Server) handleMemories(w http.ResponseWriter, r *http.Request, u *Account) {
	ms, err := s.agent.Memories(u.ID)
	if err != nil {
		writeAgentError(w, err)
		return
	}
	writeJSON(w, ms)
}

// DELETE /api/memories/{id}
//
// 成功 204。不是自己的、不存在、已经删过，都是 404（Rust 那边不区分）。
//
// 跨站检查：浏览器会自动带上 Cookie。跨站发 DELETE 本来就要先过 CORS 预检
// （我们不回 CORS 头，预检必败），SameSite=Lax 的 Cookie 也不会跟着跨站的
// fetch 走——这一道是补充，和登录、注册用的是同一个 sameOrigin。
func (s *Server) handleDeleteMemory(w http.ResponseWriter, r *http.Request, u *Account) {
	if !sameOrigin(r) {
		writeErr(w, http.StatusForbidden, "cross_origin", "跨站请求被拒绝")
		return
	}
	if err := s.agent.DeleteMemory(u.ID, r.PathValue("id")); err != nil {
		writeAgentError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

package main

// 只读调试后台：Go 查账号，聊天内容仍通过 Rust 接口读取。
import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"strconv"
)

type adminUser struct {
	ID          string `json:"id"`
	Username    string `json:"username"`
	DisplayName string `json:"display_name"`
	CreatedAt   int64  `json:"created_at"`
}

func (a *Accounts) ListAdminUsers(limit, offset int) ([]adminUser, bool, error) {
	rows, err := a.db.Query(`SELECT id,username_normalized,display_name,created_at FROM users
		ORDER BY created_at DESC,id DESC LIMIT ? OFFSET ?`, limit+1, offset)
	if err != nil {
		return nil, false, err
	}
	defer rows.Close()
	users := []adminUser{}
	for rows.Next() {
		var u adminUser
		if err := rows.Scan(&u.ID, &u.Username, &u.DisplayName, &u.CreatedAt); err != nil {
			return nil, false, err
		}
		users = append(users, u)
	}
	if err := rows.Err(); err != nil {
		return nil, false, err
	}
	hasMore := len(users) > limit
	if hasMore {
		users = users[:limit]
	}
	return users, hasMore, nil
}

// 仅用于本机的一次性 CLI 操作；没有对应 HTTP 写接口，也不修改密码。
func (a *Accounts) SetAdmin(username string) error {
	norm, err := normalizeUsername(username)
	if err != nil {
		return err
	}
	result, err := a.db.Exec(`UPDATE users SET is_admin=1 WHERE username_normalized=?`, norm)
	if err != nil {
		return err
	}
	n, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if n != 1 {
		return ErrNoSuchUser
	}
	return nil
}

func (s *Server) needAdmin(h func(http.ResponseWriter, *http.Request, *Account)) http.HandlerFunc {
	return s.needAuth(func(w http.ResponseWriter, r *http.Request, u *Account) {
		w.Header().Set("Cache-Control", "no-store")
		if !u.Admin {
			writeErr(w, http.StatusForbidden, "forbidden", "需要管理员权限")
			return
		}
		h(w, r, u)
	})
}

func adminPage(r *http.Request) (int, int, error) {
	limit, offset := 30, 0
	var err error
	if v, ok := r.URL.Query()["limit"]; ok {
		if len(v) != 1 {
			return 0, 0, fmt.Errorf("分页参数不合法")
		}
		limit, err = strconv.Atoi(v[0])
		if err != nil {
			return 0, 0, fmt.Errorf("limit 应为 1–100")
		}
	}
	if v, ok := r.URL.Query()["offset"]; ok {
		if len(v) != 1 {
			return 0, 0, fmt.Errorf("分页参数不合法")
		}
		offset, err = strconv.Atoi(v[0])
		if err != nil {
			return 0, 0, fmt.Errorf("offset 应为非负整数")
		}
	}
	if limit < 1 || limit > 100 || offset < 0 {
		return 0, 0, fmt.Errorf("分页参数不合法，limit 应为 1–100，offset 应为非负整数")
	}
	return limit, offset, nil
}

func (s *Server) handleAdminPage(w http.ResponseWriter, r *http.Request, u *Account) {
	b, err := staticFS.ReadFile("static/admin.html")
	if err != nil {
		writeErr(w, 500, "internal", "后台页面读取失败")
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("X-Frame-Options", "DENY")
	w.Write(b)
}

func (s *Server) handleAdminUsers(w http.ResponseWriter, r *http.Request, u *Account) {
	limit, offset, err := adminPage(r)
	if err != nil {
		writeErr(w, 400, "bad_request", err.Error())
		return
	}
	users, more, err := s.accounts.ListAdminUsers(limit, offset)
	if err != nil {
		writeErr(w, 500, "internal", "读取用户列表失败")
		return
	}
	writeJSON(w, map[string]any{"users": users, "has_more": more, "limit": limit, "offset": offset})
}

func (c *AgentClient) adminGet(ctx context.Context, actorID, path string) (json.RawMessage, error) {
	token, err := c.signer.signAdmin(actorID)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.base+path, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+token)
	resp, err := c.read.Do(req)
	if err != nil {
		return nil, &agentDownError{err}
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, readAgentError(resp)
	}
	var out json.RawMessage
	err = json.NewDecoder(resp.Body).Decode(&out)
	return out, err
}

func (s *Server) adminForward(w http.ResponseWriter, r *http.Request, u *Account, path string, paged bool) {
	if paged {
		limit, offset, err := adminPage(r)
		if err != nil {
			writeErr(w, 400, "bad_request", err.Error())
			return
		}
		path += fmt.Sprintf("?limit=%d&offset=%d", limit, offset)
	}
	out, err := s.agent.adminGet(r.Context(), u.ID, path)
	if err != nil {
		writeAgentError(w, err)
		return
	}
	writeJSON(w, out)
}

func (s *Server) handleAdminSessions(w http.ResponseWriter, r *http.Request, u *Account) {
	user, err := s.accounts.ByID(r.PathValue("user"))
	if errors.Is(err, ErrNoSuchUser) {
		writeErr(w, 404, "not_found", "没有这个用户")
		return
	}
	if err != nil {
		writeErr(w, 500, "internal", "读取用户失败")
		return
	}
	s.adminForward(w, r, u, "/internal/admin/users/"+url.PathEscape(user.ID)+"/sessions", true)
}
func (s *Server) handleAdminTurns(w http.ResponseWriter, r *http.Request, u *Account) {
	s.adminForward(w, r, u, "/internal/admin/sessions/"+url.PathEscape(r.PathValue("session"))+"/turns", true)
}
func (s *Server) handleAdminTurn(w http.ResponseWriter, r *http.Request, u *Account) {
	s.adminForward(w, r, u, "/internal/admin/turns/"+url.PathEscape(r.PathValue("turn")), false)
}
func (s *Server) handleAdminRaw(w http.ResponseWriter, r *http.Request, u *Account) {
	s.adminForward(w, r, u, "/internal/admin/turns/"+url.PathEscape(r.PathValue("turn"))+"/items/"+
		url.PathEscape(r.PathValue("item"))+"/raw", false)
}

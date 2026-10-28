package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/golang-jwt/jwt/v5"
)

// 不依赖 Rust 二进制，独立验证 Web 权限和账号字段的边界。
func adminFixture(t *testing.T) (*Server, *Account, *Account, string, string) {
	t.Helper()
	a, err := OpenAccounts(filepath.Join(t.TempDir(), "accounts.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { a.Close() })
	for _, name := range []string{"007a_users.sql", "008a_user_is_admin.sql"} {
		b, err := os.ReadFile(filepath.Join("..", "migrations", "accounts", name))
		if err != nil {
			t.Fatal(err)
		}
		if _, err = a.db.Exec(string(b)); err != nil {
			t.Fatal(err)
		}
	}
	admin, err := a.Create("owner", "password-for-test", true)
	if err != nil {
		t.Fatal(err)
	}
	user, err := a.Create("visitor", "password-for-test", false)
	if err != nil {
		t.Fatal(err)
	}
	at, err := a.NewSession(admin.ID)
	if err != nil {
		t.Fatal(err)
	}
	ut, err := a.NewSession(user.ID)
	if err != nil {
		t.Fatal(err)
	}
	return &Server{accounts: a, limiter: newLoginLimiter()}, admin, user, at, ut
}
func adminRequest(s *Server, path, token string) *httptest.ResponseRecorder {
	r := httptest.NewRequest(http.MethodGet, path, nil)
	if token != "" {
		r.AddCookie(&http.Cookie{Name: authCookieName, Value: token})
	}
	w := httptest.NewRecorder()
	s.routes().ServeHTTP(w, r)
	return w
}

func TestAdminRoutesRequireCurrentAdminAccount(t *testing.T) {
	s, _, user, at, ut := adminFixture(t)
	paths := []string{"/admin", "/api/admin/users", "/api/admin/users/" + user.ID + "/sessions",
		"/api/admin/sessions/s1/turns", "/api/admin/turns/t1", "/api/admin/turns/t1/items/i1/raw"}
	for _, path := range paths {
		if r := adminRequest(s, path, ""); r.Code != 401 {
			t.Errorf("anonymous %s: %d", path, r.Code)
		}
		if r := adminRequest(s, path, ut); r.Code != 403 {
			t.Errorf("user %s: %d", path, r.Code)
		}
	}
	if r := adminRequest(s, "/admin", at); r.Code != 200 || !strings.Contains(r.Body.String(), "问答调试") {
		t.Fatal("管理员无法打开页面", r.Code)
	}
	if err := s.accounts.SetAdmin("visitor"); err != nil {
		t.Fatal(err)
	}
	if r := adminRequest(s, "/api/admin/users", ut); r.Code != 200 {
		t.Fatal("已有登录应读取最新管理员标记", r.Code)
	}
	if err := s.accounts.SetAdmin("missing"); err == nil {
		t.Fatal("不存在的用户不能设置管理员")
	}
}

func TestAdminUsersPaginationAndSafeFields(t *testing.T) {
	s, _, _, at, _ := adminFixture(t)
	seen := map[string]bool{}
	for offset := 0; offset < 2; offset++ {
		r := adminRequest(s, fmt.Sprintf("/api/admin/users?limit=1&offset=%d", offset), at)
		if r.Code != 200 {
			t.Fatal(r.Code, r.Body.String())
		}
		var data struct {
			Users   []map[string]any `json:"users"`
			HasMore bool             `json:"has_more"`
		}
		if err := json.Unmarshal(r.Body.Bytes(), &data); err != nil {
			t.Fatal(err)
		}
		if len(data.Users) != 1 || data.HasMore != (offset == 0) {
			t.Fatal("分页错误", r.Body.String())
		}
		u := data.Users[0]
		id := u["id"].(string)
		if seen[id] {
			t.Fatal("分页重复用户")
		}
		seen[id] = true
		for _, key := range []string{"password_hash", "token_hash", "status", "admin", "is_admin"} {
			if _, ok := u[key]; ok {
				t.Fatalf("泄漏字段 %s", key)
			}
		}
		if r.Header().Get("Cache-Control") != "no-store" {
			t.Fatal("后台数据不应缓存")
		}
	}
	for _, query := range []string{"limit=0", "limit=101", "offset=-1", "offset=x", "limit=2&limit=3"} {
		if r := adminRequest(s, "/api/admin/users?"+query, at); r.Code != 400 {
			t.Errorf("%s: %d", query, r.Code)
		}
	}
	r := adminRequest(s, "/api/admin/users?offset=100", at)
	if !strings.Contains(r.Body.String(), `"users":[]`) {
		t.Fatal("空页应返回数组", r.Body.String())
	}
}

func TestAdminForwardSignsAdminActorAndDoesNotForwardBrowserHeaders(t *testing.T) {
	s, admin, user, at, _ := adminFixture(t)
	var paths []string
	s.agent = fakeAgent(t, func(w http.ResponseWriter, r *http.Request) {
		paths = append(paths, r.URL.RequestURI())
		token, err := jwt.Parse(strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer "),
			func(token *jwt.Token) (any, error) { return []byte(testInternalSecret), nil }, jwt.WithValidMethods([]string{"HS256"}))
		if err != nil {
			t.Error(err)
			w.WriteHeader(401)
			return
		}
		claims := token.Claims.(jwt.MapClaims)
		if claims["sub"] != admin.ID || claims["scope"] != "admin:read" {
			t.Errorf("没有签发管理员自己的范围凭证: %+v", claims)
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"test":"from-rust"}`)
	})
	for _, path := range []string{"/api/admin/users/" + user.ID + "/sessions?limit=2&offset=3",
		"/api/admin/sessions/s1/turns", "/api/admin/turns/t1", "/api/admin/turns/t1/items/i1/raw"} {
		r := httptest.NewRequest("GET", path, nil)
		r.AddCookie(&http.Cookie{Name: authCookieName, Value: at})
		r.Header.Set("Authorization", "Bearer attacker-token")
		w := httptest.NewRecorder()
		s.routes().ServeHTTP(w, r)
		if w.Code != 200 || !strings.Contains(w.Body.String(), "from-rust") {
			t.Fatal(w.Code, w.Body.String())
		}
	}
	if len(paths) != 4 || !strings.Contains(paths[0], user.ID+"/sessions?limit=2&offset=3") {
		t.Fatal("管理路径或分页丢失", paths)
	}
	if r := adminRequest(s, "/api/admin/users/missing/sessions", at); r.Code != 404 {
		t.Fatal(r.Code)
	}
	if len(paths) != 4 {
		t.Fatal("未知用户不应访问 agent")
	}
}

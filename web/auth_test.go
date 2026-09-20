package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/cookiejar"
	"net/http/httptest"
	"net/url"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

// 起一个只带账号能力的服务（不需要 Rust 二进制）。
func authServer(t *testing.T) *httptest.Server {
	t.Helper()
	bin := clipknowBin(t)
	dir := t.TempDir()
	db := filepath.Join(dir, "clipknow.db")
	if out, err := exec.Command(bin, "migrate", "--db", db).CombinedOutput(); err != nil {
		t.Fatalf("建库失败: %v\n%s", err, out)
	}
	acc, err := OpenAccounts(db)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = acc.Close() })
	ac, err := LoadAccess(defaultAccessPath(db))
	if err != nil {
		t.Fatal(err)
	}
	st, err := OpenStore(db)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = st.Close() })

	s := &Server{
		store: st, access: ac, dbPath: db, binPath: bin,
		accounts: acc, limiter: newLoginLimiter(),
	}
	srv := httptest.NewServer(s.routes())
	t.Cleanup(srv.Close)
	return srv
}

func newClient(t *testing.T) *http.Client {
	t.Helper()
	jar, err := cookiejar.New(nil)
	if err != nil {
		t.Fatal(err)
	}
	return &http.Client{Jar: jar}
}

func postJSON(t *testing.T, cli *http.Client, url, body string) *http.Response {
	t.Helper()
	resp, err := cli.Post(url, "application/json", strings.NewReader(body))
	if err != nil {
		t.Fatalf("POST %s: %v", url, err)
	}
	return resp
}

func creds(u, p string) string {
	return fmt.Sprintf(`{"username":%q,"password":%q}`, u, p)
}

func TestRegisterThenLogin(t *testing.T) {
	srv := authServer(t)
	cli := newClient(t)

	r := postJSON(t, cli, srv.URL+"/api/register", creds("alice", "hunter2hunter2"))
	defer r.Body.Close()
	if r.StatusCode != http.StatusOK {
		t.Fatalf("注册返回 %d，期望 200", r.StatusCode)
	}
	// 注册完应该直接就是登录状态
	me, err := cli.Get(srv.URL + "/api/me")
	if err != nil {
		t.Fatal(err)
	}
	defer me.Body.Close()
	if me.StatusCode != http.StatusOK {
		t.Fatalf("注册后 /api/me 返回 %d，期望 200（注册应该自动登录）", me.StatusCode)
	}
	var a Account
	_ = json.NewDecoder(me.Body).Decode(&a)
	if a.Username != "alice" {
		t.Errorf("用户名是 %q，期望 alice", a.Username)
	}
	if a.Admin {
		t.Error("注册出来的账号不该是管理员")
	}

	// 换一个干净的客户端用密码登录
	cli2 := newClient(t)
	lr := postJSON(t, cli2, srv.URL+"/api/auth/login", creds("alice", "hunter2hunter2"))
	defer lr.Body.Close()
	if lr.StatusCode != http.StatusOK {
		t.Fatalf("登录返回 %d，期望 200", lr.StatusCode)
	}
}

func TestRegisterRejectsBadInput(t *testing.T) {
	srv := authServer(t)
	for _, c := range []struct {
		name, body string
		want       int
	}{
		{"用户名太短", creds("ab", "longenoughpw"), http.StatusBadRequest},
		{"用户名有中文", creds("张三abc", "longenoughpw"), http.StatusBadRequest},
		{"用户名有空格", creds("a b c", "longenoughpw"), http.StatusBadRequest},
		{"密码太短", creds("bobbie", "short"), http.StatusBadRequest},
		{"不是 JSON", `{`, http.StatusBadRequest},
	} {
		t.Run(c.name, func(t *testing.T) {
			r := postJSON(t, newClient(t), srv.URL+"/api/register", c.body)
			defer r.Body.Close()
			if r.StatusCode != c.want {
				t.Errorf("返回 %d，期望 %d", r.StatusCode, c.want)
			}
		})
	}
}

func TestDuplicateUsernameRejected(t *testing.T) {
	srv := authServer(t)
	first := postJSON(t, newClient(t), srv.URL+"/api/register", creds("carol", "password123"))
	first.Body.Close()
	second := postJSON(t, newClient(t), srv.URL+"/api/register", creds("carol", "password123"))
	defer second.Body.Close()
	if second.StatusCode != http.StatusConflict {
		t.Errorf("重名注册返回 %d，期望 409", second.StatusCode)
	}
	// 大小写不同也算同一个人
	third := postJSON(t, newClient(t), srv.URL+"/api/register", creds("CAROL", "password123"))
	defer third.Body.Close()
	if third.StatusCode != http.StatusConflict {
		t.Errorf("大写重名返回 %d，期望 409——用户名要统一大小写", third.StatusCode)
	}
}

func TestConcurrentSameUsernameOnlyOneWins(t *testing.T) {
	// 设计文档验收表第一条：并发注册同名，最多成功一个。
	// 靠的是数据库 UNIQUE 约束，不是「先查再插」。
	srv := authServer(t)
	const n = 8
	var wg sync.WaitGroup
	codes := make([]int, n)
	for i := range n {
		wg.Add(1)
		go func() {
			defer wg.Done()
			r := postJSON(t, newClient(t), srv.URL+"/api/register", creds("dave", "password123"))
			codes[i] = r.StatusCode
			r.Body.Close()
		}()
	}
	wg.Wait()
	ok := 0
	for _, c := range codes {
		if c == http.StatusOK {
			ok++
		}
	}
	if ok != 1 {
		t.Fatalf("%d 个并发注册成功了 %d 个，只该成功 1 个：%v", n, ok, codes)
	}
}

func TestWrongPasswordAndUnknownUserLookIdentical(t *testing.T) {
	// 两者必须给出完全一样的回答，否则登录接口就成了
	// 「这个用户名注册过没有」的查询工具。
	srv := authServer(t)
	reg := postJSON(t, newClient(t), srv.URL+"/api/register", creds("erin", "password123"))
	reg.Body.Close()

	read := func(body string) (int, string) {
		r := postJSON(t, newClient(t), srv.URL+"/api/auth/login", body)
		defer r.Body.Close()
		var m map[string]string
		_ = json.NewDecoder(r.Body).Decode(&m)
		return r.StatusCode, m["code"] + "|" + m["message"]
	}
	c1, m1 := read(creds("erin", "wrongpassword"))
	c2, m2 := read(creds("nosuchuser", "wrongpassword"))
	if c1 != c2 || m1 != m2 {
		t.Errorf("密码错(%d %s) 和 用户不存在(%d %s) 的回答必须一模一样", c1, m1, c2, m2)
	}
	if c1 != http.StatusUnauthorized {
		t.Errorf("应该是 401，实际 %d", c1)
	}
}

func TestLogoutRevokesSession(t *testing.T) {
	srv := authServer(t)
	cli := newClient(t)
	reg := postJSON(t, cli, srv.URL+"/api/register", creds("frank", "password123"))
	reg.Body.Close()

	out := postJSON(t, cli, srv.URL+"/api/auth/logout", "")
	out.Body.Close()

	me, err := cli.Get(srv.URL + "/api/me")
	if err != nil {
		t.Fatal(err)
	}
	defer me.Body.Close()
	if me.StatusCode != http.StatusUnauthorized {
		t.Errorf("退出后 /api/me 返回 %d，期望 401", me.StatusCode)
	}
}

func TestStolenCookieStopsWorkingAfterLogout(t *testing.T) {
	// 退出撤销的是数据库里那条记录，不只是清浏览器 Cookie。
	// 只清 Cookie 的话，已经被偷走的令牌照样能用到过期。
	srv := authServer(t)
	cli := newClient(t)
	reg := postJSON(t, cli, srv.URL+"/api/register", creds("grace", "password123"))
	reg.Body.Close()

	var stolen string
	for _, c := range cli.Jar.Cookies(mustParse(t, srv.URL)) {
		if c.Name == authCookieName {
			stolen = c.Value
		}
	}
	if stolen == "" {
		t.Fatal("没拿到登录 Cookie")
	}

	out := postJSON(t, cli, srv.URL+"/api/auth/logout", "")
	out.Body.Close()

	// 用「偷来的」令牌直接发请求
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/api/me", nil)
	req.AddCookie(&http.Cookie{Name: authCookieName, Value: stolen})
	resp, err := (&http.Client{}).Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Errorf("退出后旧令牌还能用（返回 %d）——退出必须撤销库里那条记录", resp.StatusCode)
	}
}

func TestCrossOriginPostRejected(t *testing.T) {
	srv := authServer(t)
	req, _ := http.NewRequest(http.MethodPost, srv.URL+"/api/auth/login",
		strings.NewReader(creds("x", "y")))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Origin", "https://evil.example.com")
	resp, err := (&http.Client{}).Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusForbidden {
		t.Errorf("跨站 POST 返回 %d，期望 403", resp.StatusCode)
	}
}

func TestLoginRateLimited(t *testing.T) {
	srv := authServer(t)
	cli := newClient(t)
	hit429 := false
	for i := 0; i < loginMaxTries+5; i++ {
		r := postJSON(t, cli, srv.URL+"/api/auth/login", creds("nobody", "wrongpass1"))
		if r.StatusCode == http.StatusTooManyRequests {
			hit429 = true
			r.Body.Close()
			break
		}
		r.Body.Close()
	}
	if !hit429 {
		t.Errorf("试了 %d 次还没被限速", loginMaxTries+5)
	}
}

func mustParse(t *testing.T, raw string) *url.URL {
	t.Helper()
	u, err := url.Parse(raw)
	if err != nil {
		t.Fatal(err)
	}
	return u
}

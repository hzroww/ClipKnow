package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// 真 Go → 真 Rust → 假模型 → SQLite → 管理后台读取；不调用外部 API。
func TestAdminReadsAnotherUsersSavedTurnEndToEnd(t *testing.T) {
	bin := clipknowBin(t)
	db := filepath.Join(t.TempDir(), "clipknow.db")
	if out, err := exec.Command(bin, "migrate", "--db", db).CombinedOutput(); err != nil {
		t.Fatalf("migrate: %v %s", err, out)
	}
	a, err := OpenAccounts(db)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { a.Close() })
	admin, err := a.Create("debugowner", "password123", true)
	if err != nil {
		t.Fatal(err)
	}
	token, err := a.NewSession(admin.ID)
	if err != nil {
		t.Fatal(err)
	}
	model := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { writeFakeAnswer(t, w) }))
	t.Cleanup(model.Close)
	t.Setenv("DEEPSEEK_BASE_URL", model.URL)
	t.Setenv("DEEPSEEK_API_KEY", "fake-key-for-test")
	t.Setenv("SCRAPECREATORS_API_KEY", "fake-key-for-test")
	t.Setenv("DASHSCOPE_API_KEY", "")
	t.Setenv("CLIPKNOW_MEMORY", "off")
	agent := startAgent(t, bin, db)
	s := &Server{accounts: a, agent: agent, limiter: newLoginLimiter()}
	srv := httptest.NewServer(s.routes())
	t.Cleanup(srv.Close)
	visitor := register(t, srv, "debugvisitor")
	r := postChat(t, visitor, srv, "", "管理员需要看到的完整问题")
	events := drain(r)
	r.Body.Close()
	hello := pick(events, "hello")
	if len(hello) != 1 || len(pick(events, "done")) != 1 {
		t.Fatalf("提问失败: %+v", events)
	}
	get := func(path string) map[string]any {
		t.Helper()
		req, _ := http.NewRequest("GET", srv.URL+path, nil)
		req.AddCookie(&http.Cookie{Name: authCookieName, Value: token})
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		if resp.StatusCode != 200 {
			t.Fatalf("%s: %d", path, resp.StatusCode)
		}
		var out map[string]any
		if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
			t.Fatal(err)
		}
		return out
	}
	u, err := a.Authenticate("debugvisitor", "password123")
	if err != nil {
		t.Fatal(err)
	}
	users := get("/api/admin/users")["users"].([]any)
	if len(users) != 2 {
		t.Fatal("必须能看到全部用户", users)
	}
	sessions := get("/api/admin/users/" + u.ID + "/sessions")["sessions"].([]any)
	if len(sessions) != 1 {
		t.Fatal(sessions)
	}
	turns := get("/api/admin/sessions/" + hello[0].Session + "/turns")["turns"].([]any)
	if len(turns) != 1 {
		t.Fatal(turns)
	}
	detail := get("/api/admin/turns/" + hello[0].Turn)
	encoded, _ := json.Marshal(detail)
	if !strings.Contains(string(encoded), "管理员需要看到的完整问题") || !strings.Contains(string(encoded), fakeAnswer) {
		t.Fatal("缺少完整问题或回答", string(encoded))
	}
	resp, err := visitor.Get(srv.URL + "/api/admin/turns/" + hello[0].Turn)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 403 {
		t.Fatal("普通用户不能查看管理详情", resp.StatusCode)
	}
	// 普通内部凭证即使已经签名，也不能读取管理接口。
	var ignored any
	if err := agent.get(u.ID, "/internal/admin/turns/"+hello[0].Turn, &ignored); err == nil {
		t.Fatal("Rust 放行了普通用户凭证")
	}
	if q := get("/api/quota"); q["unlimited"] != true {
		t.Fatal("管理员应该不限次数", q)
	}
	for i := 0; i < 6; i++ {
		req, _ := http.NewRequest("POST", srv.URL+"/api/chat", strings.NewReader(fmt.Sprintf(`{"request_id":"admin-%d","question":"admin question"}`, i)))
		req.AddCookie(&http.Cookie{Name: authCookieName, Value: token})
		req.Header.Set("Content-Type", "application/json")
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		events := drain(resp)
		resp.Body.Close()
		if resp.StatusCode != 200 || len(pick(events, "done")) != 1 {
			t.Fatal("管理员提问不应被限额", resp.StatusCode, events)
		}
	}
	if q := get("/api/quota"); q["used"] != float64(0) {
		t.Fatal("管理员不扣次数", q)
	}
}

package main

// 这个文件里的测试**不需要 Rust**：用 httptest 假装一个 agent。
//
// 为什么值得单独测：Go 这一侧的职责是「把 agent 的回答翻译给浏览器」，
// 而翻译错了的表现往往是**误导**而不是报错——比如把 agent 的 401
// （两边密钥配错，一个运维问题）透传给浏览器，前端就会弹「登录过期，
// 重新输一次」，让用户去做一件完全无关的事。
//
// 这些用真 Rust 的端到端测试很难造：得故意把密钥配错、故意把 agent 杀掉。

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

// 起一个假 agent，用给定的 handler 应答。
func fakeAgent(t *testing.T, h http.HandlerFunc) *AgentClient {
	t.Helper()
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)
	c, err := NewAgentClient(srv.URL, &tokenSigner{secret: []byte(testInternalSecret)})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// 把一个 error 走一遍 writeAgentError，返回浏览器会看到的状态码和正文。
func translate(err error) (int, string) {
	w := httptest.NewRecorder()
	writeAgentError(w, err)
	return w.Code, w.Body.String()
}

func TestAgentErrorsAreTranslatedForTheBrowser(t *testing.T) {
	cases := []struct {
		name       string
		agent      int    // agent 回的状态
		code       string // agent 回的 code
		browser    int    // 浏览器该看到的
		mustSay    string // 浏览器看到的正文里必须有
		mustNotSay string // 必须没有
	}{
		{"参数错原样透传", 400, "bad_request", 400, "", ""},
		{"没找到原样透传", 404, "not_found", 404, "", ""},
		{"会话忙原样透传", 409, "session_busy", 409, "", ""},
		{"名额满归到 503", 429, "capacity", 503, "", ""},
		{"服务忙原样透传", 503, "capacity", 503, "", ""},
		// ★ 这一条是重点。agent 回 401 的含义是「Go 和 Rust 的密钥配得不
		//   一样」——一个运维问题。透传给浏览器的话，401 在前端的含义是
		//   「你的登录失效了」，于是用户被推去重新登录，白折腾。
		{"凭证不对不能透传成 401", 401, "unauthorized", 502, "配置", "登录"},
		{"上游内部错归到 502", 500, "internal", 502, "", ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			cli := fakeAgent(t, func(w http.ResponseWriter, r *http.Request) {
				w.Header().Set("Content-Type", "application/json")
				w.WriteHeader(c.agent)
				fmt.Fprintf(w, `{"code":%q,"message":"来自 agent 的原话"}`, c.code)
			})
			_, err := cli.Sessions("u1", 10)
			if err == nil {
				t.Fatal("agent 回了错误，客户端却说没事")
			}
			got, body := translate(err)
			if got != c.browser {
				t.Errorf("浏览器看到 %d，期望 %d（正文 %q）", got, c.browser, body)
			}
			if c.mustSay != "" && !strings.Contains(body, c.mustSay) {
				t.Errorf("正文里没有 %q：%s", c.mustSay, body)
			}
			if c.mustNotSay != "" && strings.Contains(body, c.mustNotSay) {
				t.Errorf("正文里不该出现 %q：%s", c.mustNotSay, body)
			}
		})
	}
}

// ★ agent 根本没起的时候，浏览器该看到 503 和一句人能看懂的话，
// 不是 500 加一串 Go 的网络错误栈。
func TestAgentDownGives503(t *testing.T) {
	// 起一个立刻关掉的服务器，拿到一个必定连不上的地址
	srv := httptest.NewServer(http.NotFoundHandler())
	url := srv.URL
	srv.Close()

	cli, err := NewAgentClient(url, &tokenSigner{secret: []byte(testInternalSecret)})
	if err != nil {
		t.Fatal(err)
	}
	_, err = cli.Sessions("u1", 10)
	if err == nil {
		t.Fatal("连不上却没报错")
	}
	code, body := translate(err)
	if code != http.StatusServiceUnavailable {
		t.Errorf("浏览器看到 %d，期望 503（正文 %q）", code, body)
	}
	if strings.Contains(body, "dial tcp") || strings.Contains(body, "connection refused") {
		t.Errorf("把 Go 的网络错误原样吐给浏览器了：%s", body)
	}
}

// agent 回了个不是 JSON 的东西（比如中间隔了个代理返回 HTML 错误页）。
// 报错要说清楚发生了什么，不能只说"解析失败"。
func TestNonJSONErrorResponseStillExplainsItself(t *testing.T) {
	cli := fakeAgent(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(502)
		fmt.Fprint(w, "<html><body>Bad Gateway</body></html>")
	})
	_, err := cli.Sessions("u1", 10)
	if err == nil {
		t.Fatal("502 却没报错")
	}
	if !strings.Contains(err.Error(), "502") || !strings.Contains(err.Error(), "Bad Gateway") {
		t.Errorf("报错里既没说状态码也没带原文：%v", err)
	}
}

// ★ 转发出去的请求必须带 Bearer 凭证，而且 user_id 在 sub 里。
func TestEveryCallCarriesTheCredential(t *testing.T) {
	var gotAuth string
	cli := fakeAgent(t, func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"sessions":[]}`)
	})
	if _, err := cli.Sessions("u_alice", 10); err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(gotAuth, "Bearer ") {
		t.Fatalf("没带 Bearer 凭证：%q", gotAuth)
	}
	// 解出 payload 确认 sub 是那个用户
	parts := strings.Split(strings.TrimPrefix(gotAuth, "Bearer "), ".")
	if len(parts) != 3 {
		t.Fatalf("不是三段的 JWT：%q", gotAuth)
	}
	raw, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	var c map[string]any
	if err := json.Unmarshal(raw, &c); err != nil {
		t.Fatal(err)
	}
	if c["sub"] != "u_alice" {
		t.Errorf("凭证里的 sub 是 %v，期望 u_alice", c["sub"])
	}
}

// ★ provider 为空时**整个字段不传**，不传一个空字符串。
//
// 传空串的话 Rust 那边收到 Some("")，白名单里没有空串，于是一个「没指定
// 模型」的正常请求变成 400「不认识的 provider: 」。这个 bug 真的发生过，
// 改造后第一次跑端到端就是它。
func TestEmptyProviderIsOmittedNotSentAsEmptyString(t *testing.T) {
	var body map[string]any
	cli := fakeAgent(t, func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"t\":\"done\",\"outcome\":\"done\",\"note\":\"\"}\n\n")
	})
	resp, err := cli.StartTurn("u1", "s1", "问题", "")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if _, present := body["provider"]; present {
		t.Errorf("provider 为空时不该出现在请求体里，实际 %#v", body)
	}

	// 给了就要传
	if _, err := cli.StartTurn("u1", "s1", "问题", "deepseek"); err != nil {
		t.Fatal(err)
	}
	if body["provider"] != "deepseek" {
		t.Errorf("provider 没传过去：%#v", body)
	}
}

// ★★ 守卫测试：web/ 目录下不许再出现任何一条聊天表的 SQL。
//
// 这条是钉「Go 不再碰 sessions / turns / items」那件事的。
//
// 为什么是 grep 而不是普通测试：「我是不是全删干净了」这个问题**测不出来**。
// 一个没人调用的查询函数不会让任何测试失败——它就静静待在那儿，直到某天
// 有人图省事又调了它，于是表结构的知识重新变成两份。
// （同一个教训吃过一次：删 append_past_answers 时漏了 sqlite.rs 里一处，
//
//	387 个测试全绿，最后是靠 grep 发现的。）
func TestGoHasNoChatTableSQL(t *testing.T) {
	// 聊天表由 Rust 拥有。users / auth_sessions 是 Go 自己的，不在此列。
	forbidden := regexp.MustCompile(
		`(?i)(from|join|into|update)\s+(sessions|turns|items)\b`)

	entries, err := os.ReadDir(".")
	if err != nil {
		t.Fatal(err)
	}
	checked := 0
	for _, e := range entries {
		name := e.Name()
		if e.IsDir() || filepath.Ext(name) != ".go" {
			continue
		}
		// 这个文件自己就写着那几个表名（在正则和注释里），跳过
		if name == "agentclient_test.go" {
			continue
		}
		b, err := os.ReadFile(name)
		if err != nil {
			t.Fatal(err)
		}
		checked++
		for i, line := range strings.Split(string(b), "\n") {
			// 注释里提到表名是可以的——那正是解释"为什么不碰它们"的地方
			trimmed := strings.TrimSpace(line)
			if strings.HasPrefix(trimmed, "//") {
				continue
			}
			if forbidden.MatchString(line) {
				t.Errorf("%s:%d 又出现了聊天表的 SQL：%s\n"+
					"这三张表由 Rust 拥有，Go 应该走 agentclient.go 问它",
					name, i+1, trimmed)
			}
		}
	}
	// 防呆：目录读空了的话上面那个循环一次都不跑，测试会假绿
	if checked < 5 {
		t.Fatalf("只检查了 %d 个 .go 文件，太少了，八成是路径不对", checked)
	}
}

package main

// 离线的端到端测试：Go → Rust 子进程 → SSE → 数据库 → 读回历史。
//
// 为什么需要这一层：
//
//	单元测试   386 个，全在 Rust 内部，碰不到 Go，更碰不到两者之间那条线
//	冒烟测试   CI 里那个，只验证服务起得来、门锁灵，从不真的问一个问题
//	评测       36 个用例真问真答，但要花钱，只能手动跑
//
// 中间空着的正是「一次完整提问的全链路」。这条链断了，上面三种都发现不了。
//
// 靠一个本地假模型服务器做到不花钱、不联网：Rust 那边 DEEPSEEK_BASE_URL
// 一换，请求就打到 httptest 起的地址上。

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/cookiejar"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// ★ 签名字符串的第三份副本（Rust 的 content::evidence::ANSWER_SIGNATURE、
// 前端 index.html 各一份）。这是已知的重复，但放一份在测试里是值得的——
// 三处不一致时，这个测试是唯一会喊出来的地方。
const answerSignature = "你好呀 我叫王浩宇 是个扫0 很开心为你福务"

// 假模型固定要说的那句话。分成几片发，顺带验证 token 事件是一片一片来的。
var fakePieces = []string{"这是", "假模型", "返回的", "固定答案"}

const fakeAnswer = "这是假模型返回的固定答案"

// 一个 OpenAI 兼容的流式服务器，永远回同一句话，从不要求调工具。
//
// 格式是照着 llm.rs 的 reassemble_stream 写的：它只认 "data: " 开头的行，
// 从 choices[0].delta.content 累加文字，靠 finish_reason 收尾，[DONE] 结束。
func fakeModel(t *testing.T, hits *int) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		*hits++
		if r.URL.Path != "/chat/completions" {
			t.Errorf("假模型收到了没想到的路径：%s", r.URL.Path)
			http.Error(w, "unexpected path", http.StatusNotFound)
			return
		}
		if got := r.Header.Get("authorization"); !strings.HasPrefix(got, "Bearer ") {
			t.Errorf("没带 Bearer 认证头，实际是 %q", got)
		}

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		flusher, ok := w.(http.Flusher)
		if !ok {
			t.Fatal("httptest 的 ResponseWriter 不支持 Flush")
		}

		send := func(v any) {
			b, _ := json.Marshal(v)
			fmt.Fprintf(w, "data: %s\n\n", b)
			flusher.Flush()
		}

		for _, piece := range fakePieces {
			send(map[string]any{"choices": []any{
				map[string]any{"index": 0, "delta": map[string]any{"content": piece}},
			}})
		}
		send(map[string]any{"choices": []any{
			map[string]any{"index": 0, "delta": map[string]any{}, "finish_reason": "stop"},
		}})
		send(map[string]any{"choices": []any{}, "usage": map[string]any{
			"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20,
		}})
		fmt.Fprint(w, "data: [DONE]\n\n")
		flusher.Flush()
	}))
}

// 找编译好的 Rust 二进制。没有就跳过——本地没跑过 cargo build 时
// 不该让整个 go test 变红，那只会让人学会忽略红色。
func clipknowBin(t *testing.T) string {
	t.Helper()
	p, err := filepath.Abs("../target/release/clipknow")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(p); err != nil {
		if _, err := exec.LookPath("cargo"); err == nil {
			t.Skip("还没编译 Rust，先跑 cargo build --release；跳过端到端测试")
		}
		t.Skip("找不到 clipknow 二进制，跳过端到端测试")
	}
	return p
}

// 起一个和 main() 一模一样的服务，只是库在临时目录里。
func startServer(t *testing.T, modelURL string) (*httptest.Server, string) {
	t.Helper()
	bin := clipknowBin(t)
	dir := t.TempDir()
	db := filepath.Join(dir, "clipknow.db")

	// 子进程继承这些（chat.go 没有设 cmd.Env）。
	// 临时目录里没有 .env，所以 Rust 那边 dotenvy 找不到真 key——
	// 这个测试**不可能**误用你的真账号，是个附带的好性质。
	t.Setenv("DEEPSEEK_BASE_URL", modelURL)
	t.Setenv("DEEPSEEK_API_KEY", "fake-key-for-test")
	t.Setenv("SCRAPECREATORS_API_KEY", "fake-key-for-test")
	t.Setenv("DASHSCOPE_API_KEY", "")

	// ★ 先把库建出来。迁移不再由服务启动时顺手做（Go 和 Rust 会同时启动，
	//   两边都跑 DDL 就是两个写者抢锁），所以这里显式跑一次——和真实部署
	//   「先 clipknow migrate 再起服务」是同一个顺序。
	mig := exec.Command(bin, "migrate", "--db", db)
	if out, err := mig.CombinedOutput(); err != nil {
		t.Fatalf("建库失败: %v\n%s", err, out)
	}

	ac, err := LoadAccess(defaultAccessPath(db))
	if err != nil {
		t.Fatalf("建不了邀请码文件: %v", err)
	}
	code, err := ac.Invite("测试用户", unlimited)
	if err != nil {
		t.Fatalf("发不了邀请码: %v", err)
	}
	st, err := OpenStore(db)
	if err != nil {
		t.Fatalf("开不了库: %v", err)
	}
	t.Cleanup(func() { _ = st.Close() })

	s := &Server{store: st, access: ac, dbPath: db, binPath: bin}
	srv := httptest.NewServer(s.routes())
	t.Cleanup(srv.Close)
	return srv, code
}

func login(t *testing.T, srv *httptest.Server, code string) *http.Client {
	t.Helper()
	jar, err := cookiejar.New(nil)
	if err != nil {
		t.Fatal(err)
	}
	cli := &http.Client{Jar: jar, Timeout: 120 * time.Second}
	body := strings.NewReader(fmt.Sprintf(`{"code":%q}`, code))
	resp, err := cli.Post(srv.URL+"/api/login", "application/json", body)
	if err != nil {
		t.Fatalf("登录请求失败: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("登录返回 %d，期望 200", resp.StatusCode)
	}
	return cli
}

type event struct {
	T    string `json:"t"`
	Text string `json:"text"`
	N    int    `json:"n"`
	// done
	Outcome string `json:"outcome"`
	Note    string `json:"note"`
	// hello
	Session string `json:"session"`
	Model   string `json:"model"`
}

// 发一个问题，把整条 SSE 流读完。
//
// session 传空字符串 = 新开一个会话（这是 /api/chat 的约定，见 chat.go 的 chatReq）。
func ask(t *testing.T, cli *http.Client, srv *httptest.Server, session, question string) []event {
	t.Helper()
	body := strings.NewReader(fmt.Sprintf(`{"session":%q,"question":%q}`, session, question))
	resp, err := cli.Post(srv.URL+"/api/chat", "application/json", body)
	if err != nil {
		t.Fatalf("提问请求失败: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("/api/chat 返回 %d，期望 200", resp.StatusCode)
	}

	var out []event
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 0, 64*1024), 4<<20)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		payload, ok := strings.CutPrefix(line, "data: ")
		if !ok {
			continue
		}
		var e event
		if err := json.Unmarshal([]byte(payload), &e); err != nil {
			t.Fatalf("SSE 里有一行不是合法 JSON: %s", payload)
		}
		out = append(out, e)
	}
	if err := sc.Err(); err != nil {
		t.Fatalf("读 SSE 流出错: %v", err)
	}
	return out
}

func pick(evs []event, kind string) []event {
	var out []event
	for _, e := range evs {
		if e.T == kind {
			out = append(out, e)
		}
	}
	return out
}

func TestChatEndToEnd(t *testing.T) {
	hits := 0
	model := fakeModel(t, &hits)
	defer model.Close()

	srv, code := startServer(t, model.URL)
	cli := login(t, srv, code)

	// ── 第一轮 ──────────────────────────────────────────
	evs := ask(t, cli, srv, "", "你好")

	if hits == 0 {
		t.Fatal("假模型一次都没被调用——请求根本没打到本地来，DEEPSEEK_BASE_URL 没生效")
	}

	hello := pick(evs, "hello")
	if len(hello) != 1 {
		t.Fatalf("hello 事件应该正好一个，实际 %d 个", len(hello))
	}
	sessionID := hello[0].Session
	if sessionID == "" {
		t.Fatal("hello 事件里没有会话 id")
	}

	// token 事件：证明文字是**边生成边转发**的，不是最后一次性给的
	tokens := pick(evs, "token")
	if len(tokens) < len(fakePieces) {
		t.Errorf("token 事件 %d 个，假模型发了 %d 片，说明流式断了",
			len(tokens), len(fakePieces))
	}
	var streamed strings.Builder
	for _, e := range tokens {
		streamed.WriteString(e.Text)
	}
	if streamed.String() != fakeAnswer {
		t.Errorf("token 拼起来是 %q，期望 %q", streamed.String(), fakeAnswer)
	}

	answers := pick(evs, "answer")
	if len(answers) != 1 {
		t.Fatalf("answer 事件应该正好一个，实际 %d 个", len(answers))
	}
	if !strings.Contains(answers[0].Text, fakeAnswer) {
		t.Errorf("answer 里没有假模型说的那句话，实际是 %q", answers[0].Text)
	}
	// 签名是在输出时拼上去的，不入库——这里是它唯一能被自动验到的地方
	if !strings.HasSuffix(strings.TrimSpace(answers[0].Text), answerSignature) {
		t.Errorf("answer 结尾没有签名，实际结尾是 %q",
			lastLine(answers[0].Text))
	}

	dones := pick(evs, "done")
	if len(dones) != 1 {
		t.Fatalf("done 事件应该正好一个，实际 %d 个", len(dones))
	}
	if dones[0].Outcome != "done" {
		t.Fatalf("outcome=%q note=%q，期望 done", dones[0].Outcome, dones[0].Note)
	}
	if len(pick(evs, "usage")) != 1 {
		t.Error("没有 usage 事件，用量统计没落下来")
	}

	// ── 落库了吗 ────────────────────────────────────────
	// 这是这个测试的重点：前面都是「流对不对」，这里是「东西真存下来了吗」。
	var list []Session
	getJSON(t, cli, srv.URL+"/api/sessions", &list)
	if len(list) != 1 {
		t.Fatalf("会话列表应该有 1 条，实际 %d 条", len(list))
	}

	var history []Message
	getJSON(t, cli, srv.URL+"/api/sessions/"+sessionID, &history)
	if len(history) != 2 {
		t.Fatalf("历史应该是「问 + 答」两条，实际 %d 条：%v", len(history), history)
	}
	if history[0].Role != "user" || !strings.Contains(history[0].Text, "你好") {
		t.Errorf("历史第一条应该是用户的提问，实际 role=%q text=%q",
			history[0].Role, history[0].Text)
	}
	if history[1].Role != "assistant" || !strings.Contains(history[1].Text, fakeAnswer) {
		t.Errorf("历史第二条应该是模型的答案，实际 role=%q text=%q",
			history[1].Role, history[1].Text)
	}
	// ★ 签名**不该**在库里。存进去的话，下一轮历史回放会让模型看到自己
	//   "说过"这句话进而模仿，压缩历史时还会把它当内容概括进去。
	if strings.Contains(history[1].Text, answerSignature) {
		t.Error("签名被存进数据库了——它只该在输出时拼，不该入库")
	}

	// ── 第二轮：接着同一个会话 ──────────────────────────
	// 带上 session id。不带的话是新开一个——那条路第一轮已经验过了。
	before := hits
	evs2 := ask(t, cli, srv, sessionID, "再问一句")
	if hits <= before {
		t.Error("第二轮没有调用模型")
	}
	if d := pick(evs2, "done"); len(d) != 1 || d[0].Outcome != "done" {
		t.Fatalf("第二轮没正常结束：%v", d)
	}
	if h2 := pick(evs2, "hello"); len(h2) != 1 || h2[0].Session != sessionID {
		t.Fatalf("第二轮的会话 id 变了：%v，期望还是 %s", h2, sessionID)
	}
	getJSON(t, cli, srv.URL+"/api/sessions/"+sessionID, &history)
	if len(history) != 4 {
		t.Errorf("两轮之后历史应该是 4 条，实际 %d 条——说明第二轮没接上同一个会话",
			len(history))
	}

	// 还是只有一个会话：确认第二轮没有偷偷新建一个
	getJSON(t, cli, srv.URL+"/api/sessions", &list)
	if len(list) != 1 {
		t.Errorf("会话列表应该还是 1 条，实际 %d 条", len(list))
	}
}

func getJSON(t *testing.T, cli *http.Client, url string, into any) {
	t.Helper()
	resp, err := cli.Get(url)
	if err != nil {
		t.Fatalf("GET %s 失败: %v", url, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET %s 返回 %d，期望 200", url, resp.StatusCode)
	}
	if err := json.NewDecoder(resp.Body).Decode(into); err != nil {
		t.Fatalf("GET %s 的响应不是合法 JSON: %v", url, err)
	}
}

func lastLine(s string) string {
	parts := strings.Split(strings.TrimSpace(s), "\n")
	return parts[len(parts)-1]
}

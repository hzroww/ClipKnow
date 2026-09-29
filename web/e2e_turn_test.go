package main

// 跨服务提问链路的端到端测试：Go → 内部 HTTP → Rust serve → agent 循环
// → 假模型 → SSE → 落库 → 读回历史。
//
// e2e_test.go 里那两个（TestChatEndToEnd / TestUsersAreIsolated）验的是
// **正常路径**。这个文件验的是改成常驻服务之后才存在的几件事：
//
//	同一会话被并发提问   → 只接受一个，另一个 409（准入登记表）
//	浏览器中途断开       → 执行照样跑完并落库（有界队列丢事件，循环不受影响）
//	模型要求调工具       → 工具结果能跨服务传回去，模型据此再答一次
//
// 全部离线：假模型是本地 httptest，工具用一个无法识别的链接，走不到网络。

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// 一个可以被"闸门"控住的假模型：收到请求后先等 release 被关掉，再回答。
//
// 用它造"一个提问正在跑"的稳定状态——靠 sleep 去卡时间窗的测试会在慢机器上
// 随机红，而 CI 的机器就是慢的。
func gatedModel(t *testing.T, release <-chan struct{}, entered chan<- struct{}) *httptest.Server {
	t.Helper()
	var once sync.Once
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		once.Do(func() { close(entered) })
		<-release
		writeFakeAnswer(t, w)
	}))
}

// 按 llm.rs 的 reassemble_stream 认得的格式吐一句固定答案。
func writeFakeAnswer(t *testing.T, w http.ResponseWriter) {
	t.Helper()
	w.Header().Set("Content-Type", "text/event-stream")
	w.WriteHeader(http.StatusOK)
	f, ok := w.(http.Flusher)
	if !ok {
		t.Error("ResponseWriter 不支持 Flush")
		return
	}
	send := func(v any) {
		b, _ := json.Marshal(v)
		fmt.Fprintf(w, "data: %s\n\n", b)
		f.Flush()
	}
	send(map[string]any{"choices": []any{
		map[string]any{"index": 0, "delta": map[string]any{"content": fakeAnswer}},
	}})
	send(map[string]any{"choices": []any{
		map[string]any{"index": 0, "delta": map[string]any{}, "finish_reason": "stop"},
	}})
	send(map[string]any{"choices": []any{}, "usage": map[string]any{
		"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20,
	}})
	fmt.Fprint(w, "data: [DONE]\n\n")
	f.Flush()
}

// 发一个提问，只返回 HTTP 响应（不读流）。调用方自己决定读多少、什么时候关。
func postChat(t *testing.T, cli *http.Client, srv *httptest.Server, session, q string) *http.Response {
	t.Helper()
	body := strings.NewReader(fmt.Sprintf(`{"session":%q,"question":%q}`, session, q))
	resp, err := cli.Post(srv.URL+"/api/chat", "application/json", body)
	if err != nil {
		t.Fatalf("提问请求失败: %v", err)
	}
	return resp
}

// ★ 同一个会话，第一个还在跑的时候再提一个 → 409。
//
// 这就是改造前 Go 里那把 turnGate 的行为，搬到 Rust 的 app::registry 之后
// 必须**一模一样**。这条测试是那次搬家的验收标准。
func TestSecondQuestionOnBusySessionIsRejected(t *testing.T) {
	release := make(chan struct{})
	entered := make(chan struct{})
	model := gatedModel(t, release, entered)
	defer model.Close()

	srv := startServer(t, model.URL)
	cli := register(t, srv, "busyuser")

	// 第一个提问：让它进到假模型里卡住
	first := postChat(t, cli, srv, "", "第一个问题")
	defer first.Body.Close()
	if first.StatusCode != http.StatusOK {
		t.Fatalf("第一个提问返回 %d，期望 200", first.StatusCode)
	}
	// 读到 hello 为止，顺便拿到会话 id
	session := readUntilHello(t, first)

	select {
	case <-entered:
	case <-time.After(30 * time.Second):
		t.Fatal("30 秒内假模型没被调用，第一个提问没真的跑起来")
	}

	// 第二个提问，同一个会话
	second := postChat(t, cli, srv, session, "第二个问题")
	defer second.Body.Close()
	if second.StatusCode != http.StatusConflict {
		b, _ := readAll(second)
		t.Errorf("同一会话的第二个提问返回 %d，期望 409。正文：%s", second.StatusCode, b)
	} else {
		b, _ := readAll(second)
		// 拒绝的话里要说清为什么，不能只有一个状态码
		if !strings.Contains(b, "还在跑") {
			t.Errorf("409 的正文没说清原因：%s", b)
		}
	}

	close(release)
	drain(first)
}

// ★ 浏览器中途断开，执行照样跑完并落库。
//
// 那一轮已经花了 SC 配额、模型 token、可能还有一次视频分析的钱，为了
// "你关了页面"把这些扔掉是最亏的。改造前靠「Go 用 exec.Command 而不是
// CommandContext」做到；改成 HTTP 之后靠的是另一套机制（有界队列丢事件、
// 循环不受影响），所以必须重新验一遍。
//
// 这条以前**没有测过**。
func TestClosingTheBrowserStillPersistsTheAnswer(t *testing.T) {
	release := make(chan struct{})
	entered := make(chan struct{})
	model := gatedModel(t, release, entered)
	defer model.Close()

	srv := startServer(t, model.URL)
	cli := register(t, srv, "gonesoon")

	resp := postChat(t, cli, srv, "", "断线之前问的")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("提问返回 %d", resp.StatusCode)
	}
	session := readUntilHello(t, resp)

	select {
	case <-entered:
	case <-time.After(30 * time.Second):
		t.Fatal("假模型没被调用")
	}

	// ★ 就在这里把连接掐掉——模型还没回答。
	resp.Body.Close()
	cli.CloseIdleConnections()

	close(release)

	// 等答案落库。轮询而不是固定 sleep：CI 的机器慢，固定睡眠要么不够
	// 要么浪费时间。
	deadline := time.Now().Add(30 * time.Second)
	for {
		var msgs []Message
		getJSON(t, cli, srv.URL+"/api/sessions/"+session, &msgs)
		if len(msgs) >= 2 && strings.Contains(msgs[len(msgs)-1].Text, fakeAnswer) {
			return // 落库了
		}
		if time.Now().After(deadline) {
			t.Fatalf("浏览器断开之后那一轮没落库，历史里只有 %d 条：%+v", len(msgs), msgs)
		}
		time.Sleep(200 * time.Millisecond)
	}
}

// ★ 模型要求调工具 → 工具结果传回去 → 模型据此再答一次。
//
// 设计文档第 11 节点名要求的用例：「至少增加一次『模型要求工具 → 工具结果
// → 再请求模型』的跨服务用例」。
//
// 工具传的是一个**无法识别的链接**，所以 fetch_video 在解析 URL 那一步就
// 失败，不碰任何网络。验的是这条链路本身通不通，不是工具干得对不对。
func TestToolCallRoundTripCrossesServices(t *testing.T) {
	var calls int
	var sawToolResult bool
	var mu sync.Mutex

	model := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Messages []struct {
				Role    string `json:"role"`
				Content any    `json:"content"`
			} `json:"messages"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)

		mu.Lock()
		calls++
		n := calls
		// 第二次调用时，历史里必须已经带上了工具结果
		if n == 2 {
			for _, m := range body.Messages {
				if m.Role == "tool" {
					sawToolResult = true
				}
			}
		}
		mu.Unlock()

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		f := w.(http.Flusher)
		send := func(v any) {
			b, _ := json.Marshal(v)
			fmt.Fprintf(w, "data: %s\n\n", b)
			f.Flush()
		}

		if n == 1 {
			// 第一次：要求调 fetch_video
			send(map[string]any{"choices": []any{map[string]any{
				"index": 0,
				"delta": map[string]any{"tool_calls": []any{map[string]any{
					"index": 0, "id": "call_1", "type": "function",
					"function": map[string]any{
						"name":      "fetch_video",
						"arguments": `{"url":"https://example.invalid/not-a-video"}`,
					},
				}}},
			}}})
			send(map[string]any{"choices": []any{map[string]any{
				"index": 0, "delta": map[string]any{}, "finish_reason": "tool_calls",
			}}})
			send(map[string]any{"choices": []any{}, "usage": map[string]any{
				"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
			}})
			fmt.Fprint(w, "data: [DONE]\n\n")
			f.Flush()
			return
		}
		// 第二次：拿到工具结果之后给答案
		writeFakeAnswer(t, w)
	}))
	defer model.Close()

	srv := startServer(t, model.URL)
	cli := register(t, srv, "tooluser")

	resp := postChat(t, cli, srv, "", "分析 https://example.invalid/not-a-video")
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("提问返回 %d", resp.StatusCode)
	}
	evs := drain(resp)

	mu.Lock()
	gotCalls, gotToolResult := calls, sawToolResult
	mu.Unlock()

	if gotCalls < 2 {
		t.Fatalf("假模型只被调了 %d 次，工具结果没有回传给它", gotCalls)
	}
	if !gotToolResult {
		t.Error("第二次请求里没有 role=tool 的消息——工具结果没进历史")
	}
	// 事件流里该看得到这次工具调用
	if len(pick(evs, "tool_call")) == 0 {
		t.Error("SSE 里没有 tool_call 事件")
	}
	if len(pick(evs, "tool_result")) == 0 {
		t.Error("SSE 里没有 tool_result 事件")
	}
	last := evs[len(evs)-1]
	if last.T != "done" {
		t.Errorf("最后一条事件是 %s，期望 done", last.T)
	}
}

// ── 小工具 ──────────────────────────────────────────────────

// 读到 hello 事件为止，返回会话 id。流不关，调用方继续用。
func readUntilHello(t *testing.T, resp *http.Response) string {
	t.Helper()
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 0, 64*1024), 4<<20)
	for sc.Scan() {
		payload, ok := strings.CutPrefix(strings.TrimSpace(sc.Text()), "data: ")
		if !ok {
			continue
		}
		var e event
		if err := json.Unmarshal([]byte(payload), &e); err != nil {
			t.Fatalf("SSE 里有一行不是合法 JSON: %s", payload)
		}
		if e.T == "hello" {
			if e.Session == "" {
				t.Fatal("hello 里没有会话 id")
			}
			return e.Session
		}
	}
	t.Fatal("流结束了也没看到 hello")
	return ""
}

// 把剩下的流读完，返回全部事件。
func drain(resp *http.Response) []event {
	var out []event
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 0, 64*1024), 4<<20)
	for sc.Scan() {
		payload, ok := strings.CutPrefix(strings.TrimSpace(sc.Text()), "data: ")
		if !ok {
			continue
		}
		var e event
		if json.Unmarshal([]byte(payload), &e) == nil {
			out = append(out, e)
		}
	}
	return out
}

func readAll(resp *http.Response) (string, error) {
	var sb strings.Builder
	sc := bufio.NewScanner(resp.Body)
	for sc.Scan() {
		sb.WriteString(sc.Text())
	}
	return sb.String(), sc.Err()
}

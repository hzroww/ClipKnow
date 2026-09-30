package main

// 跨服务提问链路的端到端测试：Go → 内部 HTTP → Rust serve → agent 循环
// → 假模型 → SSE → 落库 → 读回历史。
//
// e2e_test.go 里那两个（TestChatEndToEnd / TestUsersAreIsolated）验的是
// **正常路径**。这个文件验的是改成常驻服务之后才存在的几件事：
//
//	同一会话被并发提问   → 只接受一个，另一个 409（准入登记表）
//	不同会话同时提问     → 真的同时在跑（两个请求同时停在假模型里）
//	浏览器中途断开       → 执行照样跑完并落库（有界队列丢事件，循环不受影响）
//	模型要求调工具       → 工具结果能跨服务传回去，模型据此再答一次
//
// 全部离线：假模型是本地 httptest，工具用一个无法识别的链接，走不到网络。

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
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
	release, openGate := newGate()
	entered := make(chan struct{})
	model := gatedModel(t, release, entered)
	defer model.Close()
	// ★ 先放行、再关模型（defer 倒序执行）。测试中途失败时假模型还卡在
	//   等放行上，httptest 的 Close 要等所有请求结束——不先放行就会卡死
	//   到 go test 的 10 分钟超时，而不是立刻报错。
	defer openGate()

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

	openGate()
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
	release, openGate := newGate()
	entered := make(chan struct{})
	model := gatedModel(t, release, entered)
	defer model.Close()
	// ★ 先放行、再关模型（defer 倒序执行）。测试中途失败时假模型还卡在
	//   等放行上，httptest 的 Close 要等所有请求结束——不先放行就会卡死
	//   到 go test 的 10 分钟超时，而不是立刻报错。
	defer openGate()

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

	openGate()

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

// 一个会**数人头**的假模型：每进来一个请求就报一声，然后卡住等 release。
//
// 用它证明「真的同时在跑」：两个提问都停在模型这里 = 两个执行确实同时
// 存在。只看「两个请求都返回 200」是不够的——串行执行也能都返回 200，
// 只是第二个等了第一个。
func countingModel(t *testing.T, release <-chan struct{}, arrived chan<- int) *httptest.Server {
	t.Helper()
	var mu sync.Mutex
	inside := 0
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		inside++
		n := inside
		mu.Unlock()
		arrived <- n
		<-release
		writeFakeAnswer(t, w)
	}))
}

// 等 want 个请求同时停在假模型里。超时就说明被串行化了。
func waitAllInside(t *testing.T, arrived <-chan int, want int) {
	t.Helper()
	deadline := time.After(30 * time.Second)
	for {
		select {
		case n := <-arrived:
			if n >= want {
				return
			}
		case <-deadline:
			t.Fatalf("30 秒内没等到 %d 个提问同时进到模型里——它们被串行化了", want)
		}
	}
}

// ★ 两个人同时提问，两个执行**真的同时在跑**。
//
// 设计文档第 11 节验收表里那一行：「A、B 不同会话并发 → 能同时进入假模型，
// 不被全局单任务锁串行化」。
func TestTwoUsersAskAtTheSameTime(t *testing.T) {
	release, openGate := newGate()
	arrived := make(chan int, 8)
	model := countingModel(t, release, arrived)
	defer model.Close()
	// ★ 先放行、再关模型（defer 倒序执行）。测试中途失败时假模型还卡在
	//   等放行上，httptest 的 Close 要等所有请求结束——不先放行就会卡死
	//   到 go test 的 10 分钟超时，而不是立刻报错。
	defer openGate()

	srv := startServer(t, model.URL)
	alice := register(t, srv, "alice2")
	bob := register(t, srv, "bobby2")

	ra := postChat(t, alice, srv, "", "alice 的问题")
	defer ra.Body.Close()
	rb := postChat(t, bob, srv, "", "bob 的问题")
	defer rb.Body.Close()
	for name, r := range map[string]*http.Response{"alice": ra, "bob": rb} {
		if r.StatusCode != http.StatusOK {
			b, _ := readAll(r)
			t.Fatalf("%s 的提问返回 %d，期望 200：%s", name, r.StatusCode, b)
		}
	}

	waitAllInside(t, arrived, 2)
	openGate()

	// 两个都要正常收尾、都要落库，而且各自只看得到自己的
	for name, c := range map[string]struct {
		cli  *http.Client
		resp *http.Response
		q    string
	}{"alice": {alice, ra, "alice 的问题"}, "bob": {bob, rb, "bob 的问题"}} {
		evs := drain(c.resp)
		if len(evs) == 0 || evs[len(evs)-1].T != "done" {
			t.Errorf("%s 的流没有以 done 收尾：%+v", name, evs)
			continue
		}
		var list []Session
		getJSON(t, c.cli, srv.URL+"/api/sessions", &list)
		if len(list) != 1 || list[0].Title != c.q {
			t.Errorf("%s 的会话列表不对（并发时串台了？）：%+v", name, list)
		}
	}
}

// ★ 同一个人开两个会话同时问——现在也允许。
//
// 登记表只认会话 id，没有「每个用户最多几个」。要加的话在 app::registry
// 的 admit 里多检查一项，这条测试会提醒你改。
func TestOneUserTwoSessionsAtTheSameTime(t *testing.T) {
	release, openGate := newGate()
	arrived := make(chan int, 8)
	model := countingModel(t, release, arrived)
	defer model.Close()
	// ★ 先放行、再关模型（defer 倒序执行）。测试中途失败时假模型还卡在
	//   等放行上，httptest 的 Close 要等所有请求结束——不先放行就会卡死
	//   到 go test 的 10 分钟超时，而不是立刻报错。
	defer openGate()

	srv := startServer(t, model.URL)
	me := register(t, srv, "multitab")

	r1 := postChat(t, me, srv, "", "第一个会话的问题")
	defer r1.Body.Close()
	r2 := postChat(t, me, srv, "", "第二个会话的问题")
	defer r2.Body.Close()
	if r1.StatusCode != http.StatusOK || r2.StatusCode != http.StatusOK {
		t.Fatalf("两个会话同时提问：%d / %d，期望都是 200", r1.StatusCode, r2.StatusCode)
	}
	s1, s2 := readUntilHello(t, r1), readUntilHello(t, r2)
	if s1 == s2 {
		t.Fatal("两个新会话拿到了同一个 id")
	}

	waitAllInside(t, arrived, 2)
	openGate()
	drain(r1)
	drain(r2)

	// 两轮都落库了，各在各的会话里
	for sid, want := range map[string]string{s1: "第一个会话的问题", s2: "第二个会话的问题"} {
		var msgs []Message
		getJSON(t, me, srv.URL+"/api/sessions/"+sid, &msgs)
		if len(msgs) != 2 || msgs[0].Text != want || !strings.Contains(msgs[1].Text, fakeAnswer) {
			t.Errorf("会话 %s 的记录不对：%+v", sid[:8], msgs)
		}
	}
}

// 一个**说到一半停住**的假模型：先吐「前半句」，然后卡住等 release，
// 放行后再吐「后半句」收尾。用来造「答到一半」这个状态。
func splitModel(t *testing.T, midway chan<- struct{}, release <-chan struct{}) *httptest.Server {
	t.Helper()
	var once sync.Once
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		f := w.(http.Flusher)
		send := func(v any) {
			b, _ := json.Marshal(v)
			fmt.Fprintf(w, "data: %s\n\n", b)
			f.Flush()
		}
		delta := func(text string) {
			send(map[string]any{"choices": []any{
				map[string]any{"index": 0, "delta": map[string]any{"content": text}},
			}})
		}
		delta("前半句")
		once.Do(func() { close(midway) })
		<-release
		delta("后半句")
		send(map[string]any{"choices": []any{
			map[string]any{"index": 0, "delta": map[string]any{}, "finish_reason": "stop"},
		}})
		send(map[string]any{"choices": []any{}, "usage": map[string]any{
			"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20,
		}})
		fmt.Fprint(w, "data: [DONE]\n\n")
		f.Flush()
	}))
}

// 从一条 SSE 流里一直读，直到读到满足 stop 的那个事件（含）。
func readUntil(t *testing.T, body io.Closer, sc *bufio.Scanner, stop func(event) bool) []event {
	t.Helper()
	// ★ 等不到就 20 秒后把连接掐掉，让下面的 Scan 返回、测试报错。
	//   没有这个的话，一个「永远等不到」的事件会让测试卡到 go test 的
	//   10 分钟超时——故障注入时真卡过一次。
	timer := time.AfterFunc(20*time.Second, func() { _ = body.Close() })
	defer timer.Stop()
	var out []event
	for sc.Scan() {
		payload, ok := strings.CutPrefix(strings.TrimSpace(sc.Text()), "data: ")
		if !ok {
			continue
		}
		var e event
		if err := json.Unmarshal([]byte(payload), &e); err != nil {
			t.Fatalf("SSE 里有一行不是合法 JSON: %s", payload)
		}
		out = append(out, e)
		if stop(e) {
			return out
		}
	}
	t.Fatalf("流结束了（或 20 秒超时）也没等到想要的事件，已收到：%+v", out)
	return out
}

// ★★ 答到一半刷新页面：问题还在，而且能接着看。
//
// 这条是用户实际撞上的那个 bug 的验收：
//
//	改之前  刷新 → 会话里什么都没有（整轮答完才写库）→ 以为问题丢了
//	        后面的 token 一个都看不到，前面看到的半截也没了
//	改之后  刷新 → 看到自己的问题 + 「正在回答」
//	        重新连上 → 前半句从头补发，后半句接着实时推
func TestRefreshMidAnswerShowsQuestionAndResumes(t *testing.T) {
	midway := make(chan struct{})
	release, openGate := newGate()
	model := splitModel(t, midway, release)
	defer model.Close()
	// ★ 先放行、再关模型（defer 倒序执行）。测试中途失败时假模型还卡在
	//   等放行上，httptest 的 Close 要等所有请求结束——不先放行就会卡死
	//   到 go test 的 10 分钟超时，而不是立刻报错。
	defer openGate()

	srv := startServer(t, model.URL)
	cli := register(t, srv, "refresher")

	// ① 提问，读到模型吐出「前半句」为止
	first := postChat(t, cli, srv, "", "刷新测试的问题")
	if first.StatusCode != http.StatusOK {
		t.Fatalf("提问返回 %d", first.StatusCode)
	}
	sc := bufio.NewScanner(first.Body)
	sc.Buffer(make([]byte, 0, 64*1024), 4<<20)
	before := readUntil(t, first.Body, sc, func(e event) bool { return e.T == "token" && e.Text == "前半句" })
	session := pick(before, "hello")[0].Session

	// ② 刷新：把这条连接掐掉
	first.Body.Close()
	cli.CloseIdleConnections()

	// ③ 刷新后的页面：聊天记录里有问题，后面跟着「正在回答」
	var msgs []Message
	getJSON(t, cli, srv.URL+"/api/sessions/"+session, &msgs)
	if len(msgs) != 2 || msgs[0].Text != "刷新测试的问题" || !msgs[1].Running {
		t.Fatalf("刷新后应该看到「问题 + 正在回答」，实际：%+v", msgs)
	}
	var list []Session
	getJSON(t, cli, srv.URL+"/api/sessions", &list)
	if len(list) != 1 || !list[0].Running || list[0].Title != "刷新测试的问题" {
		t.Errorf("会话列表应该带标题并标着在跑，实际：%+v", list)
	}

	// ④ 重新连上：前半句从头补发
	resp, err := cli.Get(srv.URL + "/api/sessions/" + session + "/events")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("接着看返回 %d，期望 200", resp.StatusCode)
	}
	sc2 := bufio.NewScanner(resp.Body)
	sc2.Buffer(make([]byte, 0, 64*1024), 4<<20)
	replayed := readUntil(t, resp.Body, sc2, func(e event) bool { return e.T == "token" && e.Text == "前半句" })
	if len(pick(replayed, "hello")) != 1 {
		t.Errorf("补发里应该有开头那条 hello：%+v", replayed)
	}

	// ⑤ 放行模型：后半句要**接着实时推**过来，直到 done
	openGate()
	rest := readUntil(t, resp.Body, sc2, func(e event) bool { return e.T == "done" || e.T == "error" })
	var tail strings.Builder
	for _, e := range pick(rest, "token") {
		tail.WriteString(e.Text)
	}
	if !strings.Contains(tail.String(), "后半句") {
		t.Errorf("重新连上之后没收到后半句：%+v", rest)
	}
	if last := rest[len(rest)-1]; last.T != "done" {
		t.Fatalf("接着看的流没有以 done 收尾：%+v", last)
	}

	// ⑥ 答完：聊天记录里是完整答案，不再是「正在回答」
	getJSON(t, cli, srv.URL+"/api/sessions/"+session, &msgs)
	if len(msgs) != 2 || msgs[1].Running || !strings.Contains(msgs[1].Text, "前半句后半句") {
		t.Errorf("答完之后聊天记录不对：%+v", msgs)
	}
	// ⑦ 再来接着看：已经没在跑了 → 404，前端据此改去拉聊天记录
	again, err := cli.Get(srv.URL + "/api/sessions/" + session + "/events")
	if err != nil {
		t.Fatal(err)
	}
	again.Body.Close()
	if again.StatusCode != http.StatusNotFound {
		t.Errorf("答完之后再接着看返回 %d，期望 404", again.StatusCode)
	}
}

// ── 小工具 ──────────────────────────────────────────────────

// 一道闸门：假模型卡在它上面，测试决定什么时候放行。
// 放行可以调任意多次（第二次起什么都不做），所以既能在测试中间主动放行，
// 也能在退出时用 defer 兜底。
func newGate() (chan struct{}, func()) {
	ch := make(chan struct{})
	var once sync.Once
	return ch, func() { once.Do(func() { close(ch) }) }
}

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

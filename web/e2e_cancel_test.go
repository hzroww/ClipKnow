package main

// 停止的端到端测试：浏览器 → Go → Rust → 循环 → 假模型 / 假 ScrapeCreators。
//
// 循环在每个时刻怎么反应，Rust 的单元测试（agent/runner.rs、agent/exec.rs）
// 已经一个个钉住了。这里验的是**跨服务**才会出问题的几件事：
//
//	工具卡住时停止      很快停下；停完马上在同一个会话里再问，不能 409；
//	                    被丢下的工具还在跑时名额不还，跑完才还
//	模型正在说话时停止  让它说完（产品决定）；说完是最终答案就算正常完成
//	迟到的停止请求      拿上一轮的编号来，停不掉正在跑的新问题
//	别人的提问          停不了，404
//
// 全部离线：假模型和假 ScrapeCreators 都是本地 httptest。

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"
)

// 浏览器点停止。
func cancelTurn(t *testing.T, cli *http.Client, srv *httptest.Server, session, turn string) (int, CancelResult) {
	t.Helper()
	url := fmt.Sprintf("%s/api/sessions/%s/turns/%s/cancel", srv.URL, session, turn)
	resp, err := cli.Post(url, "application/json", nil)
	if err != nil {
		t.Fatalf("停止请求失败: %v", err)
	}
	defer resp.Body.Close()
	var out CancelResult
	if resp.StatusCode == http.StatusOK || resp.StatusCode == http.StatusAccepted {
		if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
			t.Fatalf("停止的响应不是合法 JSON: %v", err)
		}
	}
	return resp.StatusCode, out
}

// 直接问 Rust 那边全局名额用了几个。
func slotsInUse(t *testing.T, ag *AgentClient) int {
	t.Helper()
	var h struct {
		Slots int `json:"slots_in_use"`
	}
	if err := ag.get("probe", "/internal/health", &h); err != nil {
		t.Fatal(err)
	}
	return h.Slots
}

// 读到 hello 为止，返回 (会话, 这一轮的编号)，流不关。
func helloOf(t *testing.T, resp *http.Response) (*bufio.Scanner, string, string) {
	t.Helper()
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 0, 64*1024), 4<<20)
	evs := readUntil(t, resp.Body, sc, func(e event) bool { return e.T == "hello" })
	h := evs[len(evs)-1]
	if h.Session == "" || h.Turn == "" {
		t.Fatalf("hello 里缺会话或编号：%+v", h)
	}
	return sc, h.Session, h.Turn
}

// 第一次调用要求调 search_videos，之后每次都直接给答案。
func searchingModel(t *testing.T) *httptest.Server {
	t.Helper()
	var mu sync.Mutex
	calls := 0
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		calls++
		n := calls
		mu.Unlock()
		if n > 1 {
			writeFakeAnswer(t, w)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		f := w.(http.Flusher)
		send := func(v any) {
			b, _ := json.Marshal(v)
			fmt.Fprintf(w, "data: %s\n\n", b)
			f.Flush()
		}
		send(map[string]any{"choices": []any{map[string]any{
			"index": 0,
			"delta": map[string]any{"tool_calls": []any{map[string]any{
				"index": 0, "id": "call_1", "type": "function",
				"function": map[string]any{
					"name":      "search_videos",
					"arguments": `{"platform":"youtube","query":"停止测试"}`,
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
	}))
}

// 一个卡住不返回的 ScrapeCreators：收到请求先通知 entered，然后等 release。
func stuckScrapeCreators(entered chan<- struct{}, release <-chan struct{}) *httptest.Server {
	var once sync.Once
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		once.Do(func() { close(entered) })
		<-release
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"success":true,"videos":[]}`)
	}))
}

// 等 cond 成立，最多 limit。
func eventually(limit time.Duration, cond func() bool) bool {
	deadline := time.Now().Add(limit)
	for time.Now().Before(deadline) {
		if cond() {
			return true
		}
		time.Sleep(20 * time.Millisecond)
	}
	return cond()
}

// ★★ 工具卡住时停止。
//
// 这是「以后加什么工具都卡不住停止」在真实链路上的验收：工具在 Rust 那边
// 卡在一个永远不返回的 HTTP 请求上，用户点停止——
//
//	① 很快停下（循环不等工具），结局 cancelled
//	② 停完**马上**在同一个会话里再问，必须被接受（不能 409）
//	③ 被丢下的工具还在跑，全局名额还占着；它跑完了才还
func TestStopWhileToolIsStuck(t *testing.T) {
	entered := make(chan struct{})
	release, openSC := newGate()
	sc := stuckScrapeCreators(entered, release)
	defer sc.Close()
	defer openSC() // 先放行再关（defer 倒序），不然 Close 会等卡住的请求
	model := searchingModel(t)
	defer model.Close()

	t.Setenv("SCRAPECREATORS_BASE_URL", sc.URL)
	srv, ag := startServerWithAgent(t, model.URL)
	cli := register(t, srv, "stopper")

	resp := postChat(t, cli, srv, "", "帮我搜一下")
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("提问返回 %d", resp.StatusCode)
	}
	stream, session, turn := helloOf(t, resp)

	select {
	case <-entered:
	case <-time.After(30 * time.Second):
		t.Fatal("30 秒内工具没打到假 ScrapeCreators 上")
	}

	// ① 停止
	t0 := time.Now()
	code, res := cancelTurn(t, cli, srv, session, turn)
	if code != http.StatusOK || res.Status != "cancelled" {
		t.Fatalf("停止返回 %d %+v，期望 200 cancelled", code, res)
	}
	if took := time.Since(t0); took > 3*time.Second {
		t.Errorf("工具卡住时停止花了 %v——循环还在等工具", took)
	}
	rest := readUntil(t, resp.Body, stream, func(e event) bool { return e.T == "done" || e.T == "error" })
	if last := rest[len(rest)-1]; last.T != "done" || last.Outcome != "cancelled" {
		t.Errorf("事件流该以 done/cancelled 收尾：%+v", last)
	}
	var msgs []Message
	getJSON(t, cli, srv.URL+"/api/sessions/"+session, &msgs)
	if len(msgs) != 2 || !msgs[1].Cancelled || msgs[1].Failed {
		t.Errorf("聊天记录该是「问题 + 已停止」：%+v", msgs)
	}

	// ③ 工具还卡着：名额没还
	if n := slotsInUse(t, ag); n != 1 {
		t.Errorf("被丢下的工具还在跑，名额用了 %d 个，期望 1", n)
	}

	// ② 马上再问
	again := postChat(t, cli, srv, session, "那换个问法")
	defer again.Body.Close()
	if again.StatusCode != http.StatusOK {
		b, _ := readAll(again)
		t.Fatalf("停完马上再问返回 %d（%s），期望 200", again.StatusCode, b)
	}
	evs := drain(again)
	if done := pick(evs, "done"); len(done) != 1 || done[0].Outcome != "done" {
		t.Errorf("第二个问题没正常答完：%+v", evs)
	}

	// ③ 放行卡住的工具：它跑完，名额才还
	openSC()
	if !eventually(5*time.Second, func() bool { return slotsInUse(t, ag) == 0 }) {
		t.Errorf("工具跑完了名额还没还：%d", slotsInUse(t, ag))
	}
}

// ★ 模型正在说话时停止：让它说完。说完是最终答案，就算正常完成。
func TestStopWhileModelIsTalkingLetsItFinish(t *testing.T) {
	midway := make(chan struct{})
	release, openGate := newGate()
	model := splitModel(t, midway, release)
	defer model.Close()
	defer openGate()

	srv := startServer(t, model.URL)
	cli := register(t, srv, "talker")

	resp := postChat(t, cli, srv, "", "说一段话")
	defer resp.Body.Close()
	stream, session, turn := helloOf(t, resp)
	readUntil(t, resp.Body, stream, func(e event) bool { return e.T == "token" && e.Text == "前半句" })

	type result struct {
		code int
		res  CancelResult
	}
	got := make(chan result, 1)
	go func() {
		c, r := cancelTurn(t, cli, srv, session, turn)
		got <- result{c, r}
	}()

	// 模型还卡在半句话上：停止请求应该在等，不能先返回
	select {
	case r := <-got:
		t.Fatalf("模型还没说完，停止就返回了：%+v", r)
	case <-time.After(500 * time.Millisecond):
	}

	openGate()
	var r result
	select {
	case r = <-got:
	case <-time.After(15 * time.Second):
		t.Fatal("模型说完了，停止请求还没返回")
	}
	if r.code != http.StatusOK || r.res.Status != "done" {
		t.Errorf("说完的是最终答案，停止该返回 200 done，实际 %d %+v", r.code, r.res)
	}
	rest := readUntil(t, resp.Body, stream, func(e event) bool { return e.T == "done" || e.T == "error" })
	if last := rest[len(rest)-1]; last.Outcome != "done" {
		t.Errorf("该照常答完：%+v", last)
	}
	var msgs []Message
	getJSON(t, cli, srv.URL+"/api/sessions/"+session, &msgs)
	if len(msgs) != 2 || msgs[1].Text != "前半句后半句" || msgs[1].Cancelled {
		t.Errorf("聊天记录该是完整答案、不带停止标记：%+v", msgs)
	}
}

// ★ 迟到的停止请求：拿上一轮的编号来，停不掉正在跑的新问题。
func TestLateCancelDoesNotStopTheNextQuestion(t *testing.T) {
	release, openGate := newGate()
	entered := make(chan struct{})
	var mu sync.Mutex
	calls := 0
	var once sync.Once
	// 第一次直接答；之后的卡在闸门上
	model := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		calls++
		n := calls
		mu.Unlock()
		if n > 1 {
			once.Do(func() { close(entered) })
			<-release
		}
		writeFakeAnswer(t, w)
	}))
	defer model.Close()
	defer openGate()

	srv := startServer(t, model.URL)
	cli := register(t, srv, "latecomer")

	first := ask(t, cli, srv, "", "第一个问题")
	h := pick(first, "hello")[0]
	session, oldTurn := h.Session, h.Turn

	second := postChat(t, cli, srv, session, "第二个问题")
	defer second.Body.Close()
	stream, _, newTurn := helloOf(t, second)
	if newTurn == oldTurn {
		t.Fatal("两轮的编号一样")
	}
	select {
	case <-entered:
	case <-time.After(30 * time.Second):
		t.Fatal("第二个问题没进到模型里")
	}

	code, res := cancelTurn(t, cli, srv, session, oldTurn)
	if code != http.StatusOK || res.Status != "done" || res.Turn != oldTurn {
		t.Errorf("拿旧编号停止该返回旧那一轮的结局 200 done，实际 %d %+v", code, res)
	}

	openGate()
	rest := readUntil(t, second.Body, stream, func(e event) bool { return e.T == "done" || e.T == "error" })
	if last := rest[len(rest)-1]; last.Outcome != "done" {
		t.Errorf("迟到的停止请求把新问题停了：%+v", last)
	}
}

// 别人的提问停不了。
func TestCannotStopSomeoneElsesTurn(t *testing.T) {
	release, openGate := newGate()
	entered := make(chan struct{})
	model := gatedModel(t, release, entered)
	defer model.Close()
	defer openGate()

	srv := startServer(t, model.URL)
	alice := register(t, srv, "alice2")
	bob := register(t, srv, "bob2")

	resp := postChat(t, alice, srv, "", "阿的问题")
	defer resp.Body.Close()
	stream, session, turn := helloOf(t, resp)

	if code, _ := cancelTurn(t, bob, srv, session, turn); code != http.StatusNotFound {
		t.Errorf("别人停我的提问返回 %d，期望 404", code)
	}
	openGate()
	rest := readUntil(t, resp.Body, stream, func(e event) bool { return e.T == "done" || e.T == "error" })
	if last := rest[len(rest)-1]; last.Outcome != "done" {
		t.Errorf("被别人停掉了：%+v", last)
	}
}

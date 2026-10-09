package main

// 请求幂等的端到端测试：浏览器 → Go → Rust → 假模型。
//
// 浏览器点一次发送生成一个编号（request_id），自动重试带同一个。服务端按
// 「用户 + 编号」认出同一次提交：最多建一个会话、最多跑一轮。这里验的是
// 跨服务才会出问题的几件事：
//
//	答完之后重发        不重跑，返回那一轮（JSON，不是事件流）
//	正在跑的时候重发    同上，状态是 running，前端据此去「接着看」
//	两次同时到达        只跑一轮、只建一个会话
//	同一个编号换了问题  409，不能悄悄返回另一个问题的答案

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// 浏览器带着编号提问。session 为空 = 新会话。
func postChatID(t *testing.T, cli *http.Client, srv *httptest.Server, session, q, rid string) *http.Response {
	t.Helper()
	body := strings.NewReader(fmt.Sprintf(`{"session":%q,"question":%q,"request_id":%q}`, session, q, rid))
	resp, err := cli.Post(srv.URL+"/api/chat", "application/json", body)
	if err != nil {
		t.Fatalf("提问请求失败: %v", err)
	}
	return resp
}

// 重复提交时的响应体。
type dupResp struct {
	Duplicate bool   `json:"duplicate"`
	Session   string `json:"session"`
	Turn      string `json:"turn"`
	Status    string `json:"status"`
}

func isJSON(resp *http.Response) bool {
	return strings.HasPrefix(resp.Header.Get("Content-Type"), "application/json")
}

func readDup(t *testing.T, resp *http.Response) dupResp {
	t.Helper()
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK || !isJSON(resp) {
		b, _ := readAll(resp)
		t.Fatalf("期望 200 JSON（重复提交），实际 %d %s：%s",
			resp.StatusCode, resp.Header.Get("Content-Type"), b)
	}
	var d dupResp
	if err := json.NewDecoder(resp.Body).Decode(&d); err != nil {
		t.Fatal(err)
	}
	if !d.Duplicate {
		t.Fatalf("响应里没标 duplicate：%+v", d)
	}
	return d
}

// 数调用次数的假模型。release 不为 nil 时，每次调用都卡在它上面。
func countedModel(t *testing.T, calls *atomic.Int32, release <-chan struct{}) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		if release != nil {
			<-release
		}
		writeFakeAnswer(t, w)
	}))
}

func sessionCount(t *testing.T, cli *http.Client, srv *httptest.Server) int {
	t.Helper()
	var list []Session
	getJSON(t, cli, srv.URL+"/api/sessions", &list)
	return len(list)
}

// ★ 答完之后带同一个编号重发：不重跑，告诉你就是那一轮。
//
// 对应真实场景：新会话里提问，回程断了，浏览器没收到任何事件，自动重试。
// 那时第一次其实已经跑完了——没有幂等的话会再建一个会话、再付一次钱。
func TestSameRequestAfterItFinishedDoesNotRunAgain(t *testing.T) {
	var calls atomic.Int32
	model := countedModel(t, &calls, nil)
	defer model.Close()
	srv := startServer(t, model.URL)
	cli := register(t, srv, "idem1")

	first := postChatID(t, cli, srv, "", "第一次", "req-aaa")
	evs := drain(first)
	first.Body.Close()
	h := pick(evs, "hello")
	if len(h) != 1 || len(pick(evs, "done")) != 1 {
		t.Fatalf("第一次没正常跑完：%+v", evs)
	}

	// 重试：浏览器不知道会话编号（没收到 hello），所以 session 还是空的
	d := readDup(t, postChatID(t, cli, srv, "", "第一次", "req-aaa"))
	if d.Turn != h[0].Turn || d.Session != h[0].Session || d.Status != "done" {
		t.Errorf("重复提交该指向第一次那一轮：%+v，hello 是 %+v", d, h[0])
	}
	if n := calls.Load(); n != 1 {
		t.Errorf("模型被调了 %d 次，重试又跑了一遍", n)
	}
	if n := sessionCount(t, cli, srv); n != 1 {
		t.Errorf("有 %d 个会话，重试又建了一个", n)
	}
}

// ★ 正在跑的时候带同一个编号重发：状态 running，前端据此去「接着看」。
func TestSameRequestWhileRunningPointsAtTheRunningTurn(t *testing.T) {
	var calls atomic.Int32
	release, openGate := newGate()
	model := countedModel(t, &calls, release)
	defer model.Close()
	defer openGate() // 先放行再关（defer 倒序），不然 Close 会等卡住的请求
	srv := startServer(t, model.URL)
	cli := register(t, srv, "idem2")

	first := postChatID(t, cli, srv, "", "跑着的问题", "req-bbb")
	defer first.Body.Close()
	stream, session, turn := helloOf(t, first)

	d := readDup(t, postChatID(t, cli, srv, "", "跑着的问题", "req-bbb"))
	if d.Turn != turn || d.Session != session || d.Status != "running" {
		t.Errorf("该指向正在跑的那一轮：%+v（期望 %s/%s）", d, session, turn)
	}

	openGate()
	rest := readUntil(t, first.Body, stream, func(e event) bool { return e.T == "done" || e.T == "error" })
	if last := rest[len(rest)-1]; last.Outcome != "done" {
		t.Errorf("第一次该照常答完：%+v", last)
	}
	if n := calls.Load(); n != 1 {
		t.Errorf("模型被调了 %d 次", n)
	}
}

// ★★ 两次同时到达（同一个编号）：只跑一轮、只建一个会话。
//
// 这是最难的那种：两次都在对方写库之前查库，库里都查不到。靠的是两层——
// 提交记录的主键（同一个用户、同一个编号只能有一行），以及准入登记表
// 认出「正在跑的就是同一次提交」。
func TestTwoIdenticalRequestsAtTheSameTimeRunOnce(t *testing.T) {
	var calls atomic.Int32
	release, openGate := newGate()
	model := countedModel(t, &calls, release)
	defer model.Close()
	defer openGate()
	srv := startServer(t, model.URL)
	cli := register(t, srv, "idem3")

	const n = 4
	resps := make([]*http.Response, n)
	var wg sync.WaitGroup
	start := make(chan struct{})
	for i := range n {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			resps[i] = postChatID(t, cli, srv, "", "同时到达", "req-ccc")
		}()
	}
	close(start)
	wg.Wait()

	streams, dups := 0, 0
	var streamResp *http.Response
	for _, r := range resps {
		switch {
		case r.StatusCode == http.StatusOK && isJSON(r):
			dups++
			r.Body.Close()
		case r.StatusCode == http.StatusOK:
			streams++
			streamResp = r
		default:
			b, _ := readAll(r)
			r.Body.Close()
			t.Errorf("有一次被拒了：%d %s（同一次提交不该被当成「会话忙」）", r.StatusCode, b)
		}
	}
	if streams != 1 || dups != n-1 {
		t.Fatalf("期望 1 个开跑、%d 个认出是重复，实际开跑 %d、重复 %d", n-1, streams, dups)
	}
	openGate()
	drain(streamResp)
	streamResp.Body.Close()

	// 等那一轮彻底收尾再数（模型调用在 drain 之前就发生了，这里只是保险）
	time.Sleep(50 * time.Millisecond)
	if c := calls.Load(); c != 1 {
		t.Errorf("模型被调了 %d 次", c)
	}
	if c := sessionCount(t, cli, srv); c != 1 {
		t.Errorf("有 %d 个会话", c)
	}
}

// 同一个编号换了问题：409。不能悄悄返回另一个问题的那一轮。
func TestSameRequestIDWithAnotherQuestionIsRejected(t *testing.T) {
	var calls atomic.Int32
	model := countedModel(t, &calls, nil)
	defer model.Close()
	srv := startServer(t, model.URL)
	cli := register(t, srv, "idem4")

	first := postChatID(t, cli, srv, "", "问 A", "req-ddd")
	drain(first)
	first.Body.Close()

	again := postChatID(t, cli, srv, "", "问 B", "req-ddd")
	defer again.Body.Close()
	if again.StatusCode != http.StatusConflict {
		b, _ := readAll(again)
		t.Errorf("同一个编号换了问题返回 %d，期望 409：%s", again.StatusCode, b)
	}
	if n := calls.Load(); n != 1 {
		t.Errorf("模型被调了 %d 次", n)
	}
}

// 别人用同一个编号：各是各的。编号按用户划分。
func TestSameRequestIDFromAnotherUserIsIndependent(t *testing.T) {
	var calls atomic.Int32
	model := countedModel(t, &calls, nil)
	defer model.Close()
	srv := startServer(t, model.URL)
	alice := register(t, srv, "idem5a")
	bob := register(t, srv, "idem5b")

	a := postChatID(t, alice, srv, "", "同一个问题", "req-eee")
	ea := drain(a)
	a.Body.Close()
	b := postChatID(t, bob, srv, "", "同一个问题", "req-eee")
	if isJSON(b) {
		t.Fatal("波拿到了阿那一轮的重复提交响应")
	}
	eb := drain(b)
	b.Body.Close()
	ha, hb := pick(ea, "hello"), pick(eb, "hello")
	if len(ha) != 1 || len(hb) != 1 || ha[0].Session == hb[0].Session {
		t.Errorf("两人该各有各的会话：%+v / %+v", ha, hb)
	}
	if n := calls.Load(); n != 2 {
		t.Errorf("模型该被调 2 次（两个人各问一次），实际 %d", n)
	}
}

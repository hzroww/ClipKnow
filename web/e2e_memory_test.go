package main

// 长期记忆的端到端测试：浏览器 → Go → Rust → 假模型 → 库 → 记忆面板。
//
// 规则本身（原话核对、上限、替换、冻结）Rust 的单元测试已经测透了。这里验的
// 是只有整条链才会出问题的几件事：
//
//	答完之后后台真的去抽了      记忆出现在面板上（Go → Rust → memories 表）
//	新会话带着记忆              假模型收到的请求里，系统提示词之后第一条就是 <memory-context>
//	老会话不变                  已经开着的会话不会半路带上新记忆（冻结）
//	删了马上不再影响回答        删完之后，同一个会话的下一轮请求里就没有它了
//	面板按用户隔离              别人看不到、删不了；跨站发来的删除被拒

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// 用户说了这句，假模型的抽取才返回一条记忆。
const memoryTrigger = "以后都只要中文博主"

// 一个能区分两种请求的假模型：
//
//	流式（stream=true）   正常回答一轮，记下它收到的 messages
//	非流式                 记忆抽取（Rust 那边 extract_for_turn 用的是 complete）
type memoryModel struct {
	mu       sync.Mutex
	turns    [][]map[string]any
	extracts int
}

func newMemoryModel(t *testing.T) (*memoryModel, *httptest.Server) {
	t.Helper()
	m := &memoryModel{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Stream   bool             `json:"stream"`
			Messages []map[string]any `json:"messages"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Errorf("假模型收到的请求体不是 JSON: %v", err)
			http.Error(w, "bad json", http.StatusBadRequest)
			return
		}
		if body.Stream {
			m.mu.Lock()
			m.turns = append(m.turns, body.Messages)
			m.mu.Unlock()
			writeFakeAnswer(t, w)
			return
		}

		m.mu.Lock()
		m.extracts++
		m.mu.Unlock()
		// 只看「用户这一轮说的话」那段：之前说过的话也在请求里（当参考），
		// 但那里出现触发句不算——那是上一轮的事。
		ops := `{"operations":[]}`
		said := lastUserContent(body.Messages)
		if i := strings.Index(said, "<user-said>"); i >= 0 && strings.Contains(said[i:], memoryTrigger) {
			ops = fmt.Sprintf(`{"operations":[{"op":"add","category":"preference",`+
				`"content":"只要中文博主","evidence":%q}]}`, memoryTrigger)
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"choices": []any{map[string]any{
				"index":         0,
				"message":       map[string]any{"role": "assistant", "content": ops},
				"finish_reason": "stop",
			}},
			"usage": map[string]any{"prompt_tokens": 10, "completion_tokens": 5},
		})
	}))
	return m, srv
}

// 最近一次正常回答收到的 messages。
func (m *memoryModel) lastTurn(t *testing.T) []map[string]any {
	t.Helper()
	m.mu.Lock()
	defer m.mu.Unlock()
	if len(m.turns) == 0 {
		t.Fatal("假模型还没收到过正常回答的请求")
	}
	return m.turns[len(m.turns)-1]
}

func lastUserContent(msgs []map[string]any) string {
	for i := len(msgs) - 1; i >= 0; i-- {
		if msgs[i]["role"] == "user" {
			s, _ := msgs[i]["content"].(string)
			return s
		}
	}
	return ""
}

// 请求里的记忆段。规定的位置是系统提示词之后的第一条；不在那儿就当没有。
func memoryBlock(msgs []map[string]any) string {
	if len(msgs) < 2 || msgs[0]["role"] != "system" || msgs[1]["role"] != "user" {
		return ""
	}
	s, _ := msgs[1]["content"].(string)
	if !strings.HasPrefix(s, "<memory-context>") {
		return ""
	}
	return s
}

func getMemories(t *testing.T, cli *http.Client, srv *httptest.Server) Memories {
	t.Helper()
	var out Memories
	getJSON(t, cli, srv.URL+"/api/memories", &out)
	return out
}

// 抽取在后台跑，答完之后才开始。等它的结果出现在面板上，最多等 10 秒。
func waitMemories(t *testing.T, cli *http.Client, srv *httptest.Server, n int) Memories {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		got := getMemories(t, cli, srv)
		if len(got.Memories) >= n {
			return got
		}
		if time.Now().After(deadline) {
			t.Fatalf("等了 10 秒，面板上只有 %d 条记忆，期望 %d 条。后台抽取没跑，或者没写进库", len(got.Memories), n)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func deleteMemory(t *testing.T, cli *http.Client, srv *httptest.Server, id, origin string) int {
	t.Helper()
	req, err := http.NewRequest(http.MethodDelete, srv.URL+"/api/memories/"+id, nil)
	if err != nil {
		t.Fatal(err)
	}
	if origin != "" {
		req.Header.Set("Origin", origin)
	}
	resp, err := cli.Do(req)
	if err != nil {
		t.Fatalf("删除请求失败: %v", err)
	}
	resp.Body.Close()
	return resp.StatusCode
}

func TestLongTermMemoryEndToEnd(t *testing.T) {
	model, msrv := newMemoryModel(t)
	defer msrv.Close()
	srv := startServerWithMemory(t, msrv.URL)
	alice := register(t, srv, "memalice")

	// ① 第一个会话里说了一句长期偏好
	evs := ask(t, alice, srv, "", memoryTrigger+"，帮我找几个美妆博主")
	if len(pick(evs, "done")) != 1 || len(pick(evs, "hello")) != 1 {
		t.Fatalf("第一轮没正常答完：%+v", evs)
	}
	oldSession := pick(evs, "hello")[0].Session
	if blk := memoryBlock(model.lastTurn(t)); blk != "" {
		t.Errorf("还没有任何记忆，请求里却带了记忆段：%s", blk)
	}

	// ② 后台抽取，结果出现在面板上
	got := waitMemories(t, alice, srv, 1)
	m := got.Memories[0]
	if m.Category != "preference" || m.Content != "只要中文博主" || m.Evidence != memoryTrigger {
		t.Errorf("记下来的不对：%+v", m)
	}
	if got.Limits["preference"] != 10 || got.Limits["exclusion"] != 50 {
		t.Errorf("面板要的上限没带过来：%+v", got.Limits)
	}

	// ③ 新会话：系统提示词之后第一条就是记忆段
	evs = ask(t, alice, srv, "", "再帮我找几个健身博主")
	newSession := pick(evs, "hello")[0].Session
	blk := memoryBlock(model.lastTurn(t))
	if !strings.Contains(blk, "只要中文博主") || !strings.Contains(blk, "权限：低") {
		t.Fatalf("新会话该带着记忆段，实际请求最前面是：%q", blk)
	}
	if strings.Contains(blk, memoryTrigger) {
		t.Errorf("用户的原话只给面板看，不该进模型的上下文：%s", blk)
	}

	// ④ 老会话不变：它开始时还没有记忆，冻结的就是「没有」
	ask(t, alice, srv, oldSession, "那健身的呢")
	if blk := memoryBlock(model.lastTurn(t)); blk != "" {
		t.Errorf("已经开着的会话不该半路带上新记忆：%s", blk)
	}

	// ⑤ 别人看不到、删不了；跨站发来的删除被拒
	bob := register(t, srv, "membob")
	if n := len(getMemories(t, bob, srv).Memories); n != 0 {
		t.Errorf("波看到了 %d 条别人的记忆", n)
	}
	if code := deleteMemory(t, bob, srv, m.ID, ""); code != http.StatusNotFound {
		t.Errorf("波删阿的记忆返回 %d，期望 404", code)
	}
	if code := deleteMemory(t, alice, srv, m.ID, "http://evil.example"); code != http.StatusForbidden {
		t.Errorf("跨站来的删除返回 %d，期望 403", code)
	}
	if n := len(getMemories(t, alice, srv).Memories); n != 1 {
		t.Fatalf("上面两次都不该删掉它，现在剩 %d 条", n)
	}

	// ⑥ 自己删掉：面板上没了，而且刚才那个新会话的下一轮立刻不带它
	if code := deleteMemory(t, alice, srv, m.ID, ""); code != http.StatusNoContent {
		t.Fatalf("删自己的记忆返回 %d，期望 204", code)
	}
	if n := len(getMemories(t, alice, srv).Memories); n != 0 {
		t.Errorf("删完面板上还有 %d 条", n)
	}
	ask(t, alice, srv, newSession, "继续找")
	if blk := memoryBlock(model.lastTurn(t)); strings.Contains(blk, "只要中文博主") {
		t.Errorf("删掉的记忆还在影响回答：%s", blk)
	}

	model.mu.Lock()
	defer model.mu.Unlock()
	if model.extracts == 0 {
		t.Error("假模型一次抽取请求都没收到")
	}
}

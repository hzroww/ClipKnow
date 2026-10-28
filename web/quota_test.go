package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
)

func quotaForTest(t *testing.T, cli *http.Client, srv *httptest.Server) map[string]any {
	t.Helper()
	resp, err := cli.Get(srv.URL + "/api/quota")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("quota: %d", resp.StatusCode)
	}
	if resp.Header.Get("Cache-Control") != "no-store" {
		t.Fatal("quota must not be cached")
	}
	var quota map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&quota); err != nil {
		t.Fatal(err)
	}
	return quota
}

func TestFreeQuotaEndToEnd(t *testing.T) {
	var calls atomic.Int32
	model := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { calls.Add(1); writeFakeAnswer(t, w) }))
	defer model.Close()
	srv := startServer(t, model.URL)
	cli := register(t, srv, "quotauser")
	anonymous, err := http.Get(srv.URL + "/api/quota")
	if err != nil {
		t.Fatal(err)
	}
	anonymous.Body.Close()
	if anonymous.StatusCode != 401 {
		t.Fatal("anonymous quota must be rejected")
	}
	if q := quotaForTest(t, cli, srv); q["remaining"] != float64(5) {
		t.Fatal(q)
	}
	bad := postChatID(t, cli, srv, "", " ", "invalid")
	bad.Body.Close()
	if bad.StatusCode != 400 {
		t.Fatal("empty question should be rejected")
	}
	if q := quotaForTest(t, cli, srv); q["used"] != float64(0) {
		t.Fatal(q)
	}
	var firstSession string
	for i := 0; i < 5; i++ {
		resp := postChatID(t, cli, srv, "", "hello", fmt.Sprintf("r%d", i))
		if resp.StatusCode != 200 {
			t.Fatalf("question %d: %d", i, resp.StatusCode)
		}
		events := drain(resp)
		resp.Body.Close()
		hello := pick(events, "hello")
		if len(hello) != 1 || len(pick(events, "done")) != 1 {
			t.Fatal(events)
		}
		if i == 0 {
			firstSession = hello[0].Session
		}
		if q := quotaForTest(t, cli, srv); q["used"] != float64(i+1) {
			t.Fatal(q)
		}
	}
	dup := readDup(t, postChatID(t, cli, srv, "", "hello", "r4"))
	if !dup.Duplicate {
		t.Fatal("exhausted user's retry must remain idempotent")
	}
	for _, sid := range []string{"", firstSession} {
		resp := postChatID(t, cli, srv, sid, "sixth", "sixth"+sid)
		var body map[string]any
		json.NewDecoder(resp.Body).Decode(&body)
		resp.Body.Close()
		if resp.StatusCode != 429 || body["code"] != "free_quota_exhausted" {
			t.Fatalf("%d %+v", resp.StatusCode, body)
		}
	}
	if calls.Load() != 5 {
		t.Fatalf("model called %d times", calls.Load())
	}
	if q := quotaForTest(t, cli, srv); q["remaining"] != float64(0) {
		t.Fatal(q)
	}
	history, err := cli.Get(srv.URL + "/api/sessions/" + firstSession)
	if err != nil {
		t.Fatal(err)
	}
	history.Body.Close()
	if history.StatusCode != 200 {
		t.Fatal("history must remain readable")
	}
	other := register(t, srv, "quotaother")
	if q := quotaForTest(t, other, srv); q["remaining"] != float64(5) {
		t.Fatal(q)
	}
}

func TestQuotaErrorPreservesBusinessCode(t *testing.T) {
	code, body := translate(&agentError{Status: 429, Code: "free_quota_exhausted", Message: "用完了"})
	var data map[string]any
	if err := json.Unmarshal([]byte(body), &data); err != nil {
		t.Fatal(err)
	}
	if code != 429 || data["code"] != "free_quota_exhausted" {
		t.Fatal(code, body)
	}
}

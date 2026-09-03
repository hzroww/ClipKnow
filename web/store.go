package main

// 只读地查 ClipKnow 的数据库。
//
// ★ 这个文件里没有一条 INSERT / UPDATE / DELETE，而且连接是用
//   query_only(1) 打开的——写操作会在运行时被 SQLite 直接拒掉，
//   不是靠"记得别写"。
//
// 为什么 Go 不写库：
//   1. SQLite 同一时刻只允许一个写者。让写只发生在 Rust 子进程里，
//      就没有"两边抢锁"这个问题要协调。
//   2. 表结构的知识（哪些字段有不变量、摘要要在 save_turn 之后写、
//      失败的 turn 也要落库）只存在于 Rust 一处。两边都能写的话，
//      迁移一改就可能悄悄搞坏这边。

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"net/url"
	"strings"

	_ "modernc.org/sqlite"
)

type Store struct{ db *sql.DB }

// 一个会话。
type Session struct {
	ID        string `json:"id"`
	Title     string `json:"title"`
	CreatedAt int64  `json:"created_at"`
}

// 聊天记录里的一条。数据库里一次提问存 4~5 条（问题 / 中间思考 /
// 工具调用 / 工具结果 / 最终答案），这里只留两条：问题和最终答案。
// 中间那些是过程，不是对话。
type Message struct {
	Role string `json:"role"` // "user" | "assistant"
	Text string `json:"text"`
	Seq  int64  `json:"seq"` // 第几次提问

	// 这一轮是失败收场的（截断 / 撞迭代上限 / 协议异常）。
	//
	// ⚠️ 只有布尔，没有原因——`turns.status` 只存 "done"/"failed"，
	// Rust 侧 `TurnStatus::Failed(String)` 里那个原因**根本没落库**。
	// 实时看的时候能从 done 事件的 note 里看到，刷新之后就只剩这个布尔了。
	// 要补的话是 turns 加一列 + 一次迁移。
	Failed bool `json:"failed"`
}

func OpenStore(path string) (*Store, error) {
	// query_only：这条连接上的任何写操作都会被 SQLite 拒绝。
	// busy_timeout：Rust 子进程正在写时，这边等而不是立刻失败。
	dsn := fmt.Sprintf(
		"file:%s?_pragma=query_only(1)&_pragma=busy_timeout(5000)",
		url.PathEscape(path),
	)
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	if err := db.Ping(); err != nil {
		return nil, fmt.Errorf("打不开数据库 %s: %w", path, err)
	}
	return &Store{db: db}, nil
}

func (s *Store) Close() error { return s.db.Close() }

// 库还没建表——**全新安装的正常状态**，不是错误。
//
// 建表是 Rust 那边 init() 跑迁移做的，而 Go 是只读打开的（query_only(1)），
// 所以在第一次提问之前这些表根本不存在。实测：全新装好打开网页，会话列表
// 直接 500「no such table: sessions」，而正确的显示是「一条会话都没有」。
//
// 不在 Go 里建表是刻意的：写只发生在 Rust 子进程里，这样就没有两个进程抢
// SQLite 写锁的问题。所以这里认下这个错，返回空。
func isNoSchema(err error) bool {
	return err != nil && strings.Contains(err.Error(), "no such table")
}

// 会话列表，新的在前。
func (s *Store) Sessions(limit int) ([]Session, error) {
	rows, err := s.db.Query(
		`SELECT id, COALESCE(title, ''), created_at
		 FROM sessions ORDER BY created_at DESC LIMIT ?`, limit)
	if isNoSchema(err) {
		return []Session{}, nil
	}
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	// 空切片而不是 nil：nil 会被 encoding/json 序列化成 null，
	// 前端 .map() 直接报错。
	out := []Session{}
	for rows.Next() {
		var v Session
		if err := rows.Scan(&v.ID, &v.Title, &v.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, rows.Err()
}

// 一个会话的聊天记录。
//
// 每个 turn 取两样：user_message（你问的），以及**最后一条**
// assistant_message（最终答案）。前面那些 assistant_message 是模型在
// 调工具之前的中间思考，界面上不显示。
func (s *Store) History(sessionID string) ([]Message, error) {
	rows, err := s.db.Query(
		`SELECT t.seq, t.status, i.item_type, i.payload_json
		 FROM turns t
		 JOIN items i ON i.turn_id = t.id
		 WHERE t.session_id = ?
		   AND i.item_type IN ('user_message', 'assistant_message')
		 ORDER BY t.seq, i.idx`, sessionID)
	if isNoSchema(err) {
		return []Message{}, nil
	}
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	out := []Message{}
	// 每个 turn 的最后一条 assistant 在 out 里的下标，-1 表示这个 turn
	// 还没出现过 assistant。用下标覆盖而不是先收集再挑，是为了让
	// 「问题在前、答案在后」的顺序自然保持住。
	lastAssistant := -1
	curSeq := int64(-1)

	for rows.Next() {
		var seq int64
		var status, kind, payload string
		if err := rows.Scan(&seq, &status, &kind, &payload); err != nil {
			return nil, err
		}
		if seq != curSeq {
			curSeq, lastAssistant = seq, -1
		}

		var p struct {
			Text string `json:"text"`
		}
		// 解析失败不要整条记录都丢掉——宁可显示空文本，
		// 也别让一条坏数据把整个会话变成打不开
		_ = json.Unmarshal([]byte(payload), &p)

		switch kind {
		case "user_message":
			out = append(out, Message{Role: "user", Text: p.Text, Seq: seq})
		case "assistant_message":
			m := Message{Role: "assistant", Text: p.Text, Seq: seq, Failed: status != "done"}
			if lastAssistant >= 0 {
				out[lastAssistant] = m // 覆盖掉上一条中间思考
			} else {
				lastAssistant = len(out)
				out = append(out, m)
			}
		}
	}
	return out, rows.Err()
}

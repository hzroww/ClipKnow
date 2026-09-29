package main

// ⚠️ 这个文件是**过渡期的残留**，只剩一个查询，下一个提交会整个删掉。
//
// 它原来有三个查询：会话列表（Sessions）、聊天记录（History）、归属检查
// （OwnsSession）。前两个已经搬到 Rust 那边了（src/app/sessions.rs），
// Go 改成通过 HTTP 问（web/agentclient.go）。
//
// 只剩 OwnsSession 还在这里，是因为 chat.go 目前还在用 exec.Command 起
// `clipknow turn` 子进程——那条路还没搬，而它在花钱之前需要确认「这个会话
// 是不是你的」。等提问也走 HTTP 之后（归属检查就发生在 Rust 那一侧），
// 这个文件和 Go 对聊天表的最后一条 SQL 一起消失。
//
// 为什么非搬不可：sessions / turns / items 三张表由 Rust 写、Go 读，
// **两边都得懂表结构**。改一列要改两个地方，漏一个就是线上 bug。

import (
	"database/sql"
	"fmt"
	"net/url"
	"strings"

	_ "modernc.org/sqlite"
)

type Store struct{ db *sql.DB }

func OpenStore(path string) (*Store, error) {
	// query_only：这条连接上的任何写操作都会被 SQLite 拒绝。
	// busy_timeout：Rust 那边正在写时，这边等而不是立刻失败。
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
// 建表是 `clipknow migrate` 做的，而 Go 是只读打开的（query_only(1)），
// 所以在迁移跑过之前这些表根本不存在。实测：全新装好打开网页，会话列表
// 直接 500「no such table: sessions」，而正确的显示是「一条会话都没有」。
func isNoSchema(err error) bool {
	return err != nil && strings.Contains(err.Error(), "no such table")
}

// 这个会话是不是这个用户的（且没被删）。
//
// chat.go 接着某个会话提问时要先问这一句。返回 false 的情况包括「不存在」
// 和「是别人的」——调用方一律按未找到处理，不区分。区分开等于告诉对方
// 「有这么个会话，只是不给你看」，那本身就是泄漏。
func (s *Store) OwnsSession(userID, sessionID string) (bool, error) {
	var n int
	err := s.db.QueryRow(
		`SELECT count(*) FROM sessions
		 WHERE id = ? AND user_id = ? AND deleted_at IS NULL`,
		sessionID, userID).Scan(&n)
	if isNoSchema(err) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	return n > 0, nil
}

package main

// 一次性的导入：把邀请码时代的三个「码」变成真正的账号。
//
// ## 为什么分成两步
//
// 账号表是 Go 的，sessions 是 Rust 的。所以：
//
//	这一步（Go）      建 users，把 user_id 写回 access.json
//	下一步（Rust）    clipknow claim-sessions 读 access.json，填 sessions.user_id
//
// 一个命令干完当然更省事，但那样 Go 就得写 Rust 的表。这是**一次性的离线
// 工具**，破例一次没人会发现——然后下一个人就照着破例了。边界是靠不破例
// 维持的。
//
// ## 密码
//
// 随机生成，只在这次输出里打印一遍，不写进任何文件。设计文档要求「不记录或
// 恢复明文密码」——忘了就由运维重置，不是从哪里捞回来。

import (
	"crypto/rand"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// 初始密码用的字符集。去掉了容易看错的 0/O/1/l/I——这串要念给人听或者手打。
const pwAlphabet = "23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ"

func randomPassword(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		panic("拿不到随机数: " + err.Error())
	}
	var sb strings.Builder
	for _, v := range b {
		sb.WriteByte(pwAlphabet[int(v)%len(pwAlphabet)])
	}
	return sb.String()
}

// 从邀请码推一个合法用户名。
//
// 现有的三个名字是「我」「朋友」「朋友」——中文，而且有重复，推不出唯一的
// ASCII 用户名。所以用码本身：u427gs6p5 这样，丑但唯一，而且一眼能对上是
// 哪个码。想要好看的名字，之后改显示名称或者重新注册。
func usernameFromCode(code string) string {
	return "u" + strings.ToLower(code)
}

// access.json 里我们要读的那部分。
//
// 邀请码那套代码已经删了，但**这个一次性导入工具还要读那个老文件**，
// 所以在这里留一份最小的结构。等所有人都导入完、老文件可以扔掉时，
// 这个文件整个删掉。
type accessUser struct {
	Name   string `json:"name"`
	Admin  bool   `json:"admin"`
	UserID string `json:"user_id,omitempty"`
}

type accessState struct {
	Users map[string]*accessUser `json:"users"`
	Owner map[string]string      `json:"owner"`
}

// access.json 默认就在库旁边。
func defaultAccessPath(dbPath string) string {
	return filepath.Join(filepath.Dir(dbPath), "access.json")
}

type importedAccount struct {
	Code     string
	Username string
	Password string
	UserID   string
	Admin    bool
}

// 读 access.json，给每个码建一个账号，把 user_id 写回文件。
func importAccounts(accessPath, dbPath string) error {
	raw, err := os.ReadFile(accessPath)
	if err != nil {
		return fmt.Errorf("读不了 %s: %w", accessPath, err)
	}
	var st accessState
	if err := json.Unmarshal(raw, &st); err != nil {
		return fmt.Errorf("%s 不是合法 JSON: %w", accessPath, err)
	}

	acc, err := OpenAccounts(dbPath)
	if err != nil {
		return err
	}
	defer acc.Close()

	// 码在 map 里顺序随机，排一下，让每次输出可复现
	codes := make([]string, 0, len(st.Users))
	for c := range st.Users {
		codes = append(codes, c)
	}
	sort.Strings(codes)

	var made []importedAccount
	for _, code := range codes {
		u := st.Users[code]
		if u.UserID != "" {
			fmt.Printf("  跳过 %s（%s）：已经导入过，user_id=%s\n", code, u.Name, u.UserID)
			continue
		}
		username := usernameFromCode(code)
		pw := randomPassword(12)
		a, err := acc.Create(username, pw, u.Admin)
		if err != nil {
			return fmt.Errorf("建账号 %s 失败: %w", username, err)
		}
		// 显示名称沿用原来的中文昵称
		if _, err := acc.db.Exec(
			`UPDATE users SET display_name = ? WHERE id = ?`, u.Name, a.ID); err != nil {
			return err
		}
		u.UserID = a.ID
		made = append(made, importedAccount{code, username, pw, a.ID, u.Admin})
	}

	if len(made) == 0 {
		fmt.Println("没有需要导入的码。")
		return nil
	}

	// ★ 先把 user_id 写回文件再打印密码。
	//   反过来的话，写文件失败时用户已经看到了一串「有效」的密码，
	//   而库里其实没有对应关系，下一步认领会话会全落空。
	out, err := json.MarshalIndent(st, "", "  ")
	if err != nil {
		return err
	}
	tmp := accessPath + ".tmp"
	if err := os.WriteFile(tmp, out, 0o600); err != nil {
		return err
	}
	if err := os.Rename(tmp, accessPath); err != nil {
		return err
	}

	fmt.Printf("\n建了 %d 个账号。**初始密码只在这里显示这一次**，不会写进任何文件：\n\n", len(made))
	fmt.Printf("  %-10s %-12s %-14s %s\n", "原邀请码", "用户名", "初始密码", "管理员")
	for _, m := range made {
		admin := ""
		if m.Admin {
			admin = "是"
		}
		fmt.Printf("  %-10s %-12s %-14s %s\n", m.Code, m.Username, m.Password, admin)
	}
	fmt.Printf("\n下一步把历史会话归到这些账号名下：\n")
	fmt.Printf("  clipknow claim-sessions --db %s --access %s\n", dbPath, accessPath)
	return nil
}

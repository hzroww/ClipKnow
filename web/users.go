package main

// 账号存储。**Go 拥有 users / auth_sessions 两张表**，Rust 运行时不读写它们。
//
// ## 为什么单开一条可写连接
//
// store.go 那条是 query_only(1) 打开的，写操作会被 SQLite 直接拒掉。那条约束
// 要留着——它保证「Agent 的表只有 Rust 写」。但账号表是 Go 的，Go 必须能写。
//
// 所以分成两条连接，各自只碰自己的表：
//
//	Store     只读  sessions / turns / items / 视频资料
//	Accounts  可写  users / auth_sessions
//
// 共用一个数据库文件只是部署选择。拥有权是靠「谁拿着哪条连接」表达的，
// 不是靠「记得别写」。

import (
	"database/sql"
	"errors"
	"fmt"
	"net/url"
	"strings"
	"time"

	"github.com/google/uuid"
	_ "modernc.org/sqlite"
)

type Accounts struct {
	db *sql.DB
}

// 一个账号。
//
// 叫 Account 不叫 User，是因为库里那张表叫 users——两个名字对着看，一眼能分清
// 「内存里的结构」和「库里的行」。而且 password_hash 不在这里，它只在
// Create / Authenticate 内部出现，不会顺手被 JSON 序列化出去。
type Account struct {
	ID          string `json:"id"`
	Username    string `json:"username"`     // 规范化后的
	DisplayName string `json:"display_name"` // 用户自己写的大小写
	Status      string `json:"status"`
	Admin       bool   `json:"admin"`
}

var (
	ErrUsernameTaken = errors.New("这个用户名已经有人用了")
	ErrBadUsername   = errors.New("用户名只能用字母、数字、下划线和短横线，3–32 个字符")
	ErrBadPassword   = errors.New("密码至少 8 个字符")
	ErrNoSuchUser    = errors.New("用户名或密码不对")
	ErrUserDisabled  = errors.New("这个账号已被停用")
)

func OpenAccounts(path string) (*Accounts, error) {
	// 不带 query_only：这条连接要写账号表。
	// busy_timeout 照旧——Rust 子进程可能正在写它自己的表。
	dsn := fmt.Sprintf("file:%s?_pragma=busy_timeout(5000)&_pragma=foreign_keys(1)",
		url.PathEscape(path))
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	if err := db.Ping(); err != nil {
		return nil, fmt.Errorf("打不开账号库 %s: %w", path, err)
	}
	return &Accounts{db: db}, nil
}

func (a *Accounts) Close() error { return a.db.Close() }

// 用户名规范化。
//
// ★ 第一版只收 ASCII。中文用户名要处理 Unicode 规范化（同一个字有多种编码
// 方式）和同形异码攻击（拿形状一样的字符冒充别人），那是个能单独做很久的
// 题目。先划一条明确的线，显示名称另存，不影响你用中文昵称。
func normalizeUsername(raw string) (string, error) {
	s := strings.ToLower(strings.TrimSpace(raw))
	if len(s) < 3 || len(s) > 32 {
		return "", ErrBadUsername
	}
	for _, r := range s {
		ok := (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') || r == '_' || r == '-'
		if !ok {
			return "", ErrBadUsername
		}
	}
	return s, nil
}

// 建一个用户。
//
// ★ 重名靠数据库的 UNIQUE 约束拦，不靠「先查一下没有再插入」——两步之间有
// 竞态窗口，两个并发注册会双双通过检查。这里直接插，撞了就把约束错误翻译
// 成人话。
func (a *Accounts) Create(displayName, password string, admin bool) (*Account, error) {
	norm, err := normalizeUsername(displayName)
	if err != nil {
		return nil, err
	}
	if len([]rune(password)) < 8 {
		return nil, ErrBadPassword
	}
	hash, err := hashPassword(password)
	if err != nil {
		return nil, err
	}
	now := time.Now().Unix()
	u := &Account{
		ID: uuid.NewString(), Username: norm, DisplayName: displayName,
		Status: "active", Admin: admin,
	}
	_, err = a.db.Exec(
		`INSERT INTO users(id, username_normalized, display_name, password_hash,
		                   status, is_admin, created_at, updated_at)
		 VALUES (?,?,?,?,?,?,?,?)`,
		u.ID, u.Username, u.DisplayName, hash, u.Status, boolToInt(admin), now, now)
	if err != nil {
		if strings.Contains(err.Error(), "UNIQUE") {
			return nil, ErrUsernameTaken
		}
		return nil, err
	}
	return u, nil
}

// 校验用户名密码。
//
// ★ 用户名不存在和密码不对返回**同一个**错误。分开报的话，攻击者能拿登录
// 接口当「这个用户名注册了没有」的查询器。
func (a *Accounts) Authenticate(username, password string) (*Account, error) {
	norm, err := normalizeUsername(username)
	if err != nil {
		return nil, ErrNoSuchUser
	}
	var u Account
	var hash string
	var admin int
	err = a.db.QueryRow(
		`SELECT id, username_normalized, display_name, password_hash, status, is_admin
		 FROM users WHERE username_normalized = ?`, norm).
		Scan(&u.ID, &u.Username, &u.DisplayName, &hash, &u.Status, &admin)
	if errors.Is(err, sql.ErrNoRows) {
		// ★ 即使用户不存在也算一次哈希再返回。
		//   直接返回的话，「不存在」是微秒级、「密码错」是几十毫秒级，
		//   拿响应时间就能枚举出哪些用户名注册过。
		_ = verifyPassword(password, "$argon2id$v=19$m=65536,t=3,p=2$"+
			"AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
		return nil, ErrNoSuchUser
	}
	if err != nil {
		return nil, err
	}
	if !verifyPassword(password, hash) {
		return nil, ErrNoSuchUser
	}
	if u.Status != "active" {
		return nil, ErrUserDisabled
	}
	u.Admin = admin != 0
	return &u, nil
}

func (a *Accounts) ByID(id string) (*Account, error) {
	var u Account
	var admin int
	err := a.db.QueryRow(
		`SELECT id, username_normalized, display_name, status, is_admin
		 FROM users WHERE id = ?`, id).
		Scan(&u.ID, &u.Username, &u.DisplayName, &u.Status, &admin)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNoSuchUser
	}
	if err != nil {
		return nil, err
	}
	u.Admin = admin != 0
	return &u, nil
}

func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

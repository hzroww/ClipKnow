package main

// 邀请码 + 配额 + 会话归属。
//
// ## 为什么是「每人一个码」而不是「一个共享密码」
//
// 需求是「每人最多问 5 个问题」。一个共享密码分不清谁是谁，也就没法分别
// 计数。码就是身份，一个机制同时解决三件事：
//   登录     —— 没码进不来
//   配额     —— 每个码单独记剩几次
//   会话隔离 —— 只看得见自己的会话
// 附带好处：某个人滥用了，删掉他那一个码就行，不影响别人。
//
// ## 状态存在哪
//
// 一个独立的 JSON 文件，**不是 clipknow.db**。
//
// Go 对主库是只读的（连接用 query_only(1) 打开），这条原则要保住：写只发生
// 在 Rust 子进程里，就没有两个进程抢 SQLite 写锁的问题。而「谁还剩几次」
// 是 web 这一层自己的状态，跟视频资料没关系，本来也不该混进那个库。
//
// ## 这套东西挡得住什么，挡不住什么
//
// 挡得住：不知道码的人用掉你的钱。这是当前唯一真正要防的。
// 挡不住：朋友之间互相转发码。三个朋友的场景不值得为此做设备绑定之类的东西。

import (
	"crypto/rand"
	"crypto/subtle"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const (
	cookieName = "ck_code"
	// 无限次。留给你自己用，免得自己测试时被自己的配额挡住。
	unlimited = -1
	// 登录失败时故意慢一点。三个朋友 + 随机隧道地址的场景不需要正经的限流，
	// 但让暴力猜码从「每秒几千次」变成「每秒几次」几乎不要钱。
	loginFailDelay = 300 * time.Millisecond
)

type User struct {
	Name  string `json:"name"`
	Quota int    `json:"quota"` // 剩余提问次数；unlimited 表示不限
	Admin bool   `json:"admin"` // 看得见所有会话（包括命令行建的那些）
}

type accessState struct {
	Users map[string]*User  `json:"users"` // 邀请码 → 人
	Owner map[string]string `json:"owner"` // 会话 id → 邀请码
}

type Access struct {
	mu    sync.Mutex
	path  string
	state accessState
}

func LoadAccess(path string) (*Access, error) {
	a := &Access{path: path}
	b, err := os.ReadFile(path)
	switch {
	case os.IsNotExist(err):
		// 第一次跑：生成一个管理员码，打到日志里
		code := newCode()
		a.state = accessState{
			Users: map[string]*User{code: {Name: "我", Quota: unlimited, Admin: true}},
			Owner: map[string]string{},
		}
		if err := a.save(); err != nil {
			return nil, err
		}
		fmt.Printf("\n★ 第一次运行，已生成你自己的邀请码：%s\n"+
			"  （存在 %s，加朋友用 -invite 参数）\n\n", code, path)
	case err != nil:
		return nil, err
	default:
		if err := json.Unmarshal(b, &a.state); err != nil {
			return nil, fmt.Errorf("%s 解析失败: %w", path, err)
		}
		if a.state.Users == nil {
			a.state.Users = map[string]*User{}
		}
		if a.state.Owner == nil {
			a.state.Owner = map[string]string{}
		}
	}
	return a, nil
}

// 从文件重读一遍。
//
// ★ 每个操作前都读，而不是启动时读一次缓存住。
//
//	因为 `-invite` 是**另一个进程**在写同一个文件：缓存的话，服务既看不到
//	新发的码，还会在下次保存时把它覆盖掉。实测过——发完码登录直接「邀请码
//	不对」，而文件里明明有。
//	文件就几百字节，两三个人的量级下这点读开销可以忽略。
func (a *Access) reload() {
	b, err := os.ReadFile(a.path)
	if err != nil {
		return // 文件暂时读不到（比如正在被重命名）就用内存里的
	}
	var st accessState
	if json.Unmarshal(b, &st) != nil {
		return // 半截的 JSON，别拿它覆盖内存里好的那份
	}
	if st.Users == nil {
		st.Users = map[string]*User{}
	}
	if st.Owner == nil {
		st.Owner = map[string]string{}
	}
	a.state = st
}

// 写文件。**先写临时文件再重命名**——直接覆盖的话，写到一半断电就得到一个
// 半截的 JSON，下次启动谁都进不来。
func (a *Access) save() error {
	b, err := json.MarshalIndent(a.state, "", "  ")
	if err != nil {
		return err
	}
	tmp := a.path + ".tmp"
	if err := os.WriteFile(tmp, b, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, a.path)
}

// 新建一个邀请码。返回码本身。
func (a *Access) Invite(name string, quota int) (string, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.reload()
	code := newCode()
	a.state.Users[code] = &User{Name: name, Quota: quota}
	return code, a.save()
}

// 码对应的人。码不对返回 nil。
func (a *Access) lookup(code string) *User {
	if code == "" {
		return nil
	}
	// 常数时间比较：普通的 map 查找会因为字符串比较提前返回而泄漏一点时机，
	// 这里几乎不花钱，就做了。
	for k, u := range a.state.Users {
		if subtle.ConstantTimeCompare([]byte(k), []byte(code)) == 1 {
			return u
		}
	}
	return nil
}

// 从请求的 cookie 认人。没登录返回 ("", nil)。
func (a *Access) From(r *http.Request) (string, *User) {
	c, err := r.Cookie(cookieName)
	if err != nil {
		return "", nil
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	a.reload()
	if u := a.lookup(c.Value); u != nil {
		return c.Value, u
	}
	return "", nil
}

// 扣一次配额。返回是否放行。
//
// ★ 在**起子进程之前**扣，不是跑完再扣。一次提问一旦开始就已经在花钱了
// （SC 调用、模型 token、可能还有视频分析），哪怕最后失败。按「开始」计费
// 才对得上实际支出。代价是：极少数在花钱之前就失败的情况（数据库打不开）
// 也会扣一次——需要的话你手改 access.json 补回去就行。
func (a *Access) Spend(code string) (ok bool, left int) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.reload()
	u := a.lookup(code)
	if u == nil {
		return false, 0
	}
	if u.Quota == unlimited {
		return true, unlimited
	}
	if u.Quota <= 0 {
		return false, 0
	}
	u.Quota--
	_ = a.save()
	return true, u.Quota
}

// 记下会话属于谁。Rust 在 hello 那一行报出会话 id，Go 转发时顺手记一笔。
func (a *Access) Claim(sessionID, code string) {
	if sessionID == "" || code == "" {
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	a.reload()
	if a.state.Owner[sessionID] == code {
		return // 已经记过了，别为每一轮都写一次文件
	}
	a.state.Owner[sessionID] = code
	_ = a.save()
}

// 这个人能不能看这个会话。
//
// 管理员看得见全部，包括**命令行建的那些**——它们在 Owner 里没有记录，
// 而那些正是你自己的历史会话。普通用户只看得见自己认领过的。
func (a *Access) CanSee(sessionID, code string, u *User) bool {
	if u.Admin {
		return true
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	a.reload()
	return a.state.Owner[sessionID] == code
}

// 邀请码：8 位，去掉了容易看错的 0/O/1/l/I。
// 朋友要照着念或者手打，少一个歧义字符少一次「你那个是零还是欧」。
func newCode() string {
	const alphabet = "23456789ABCDEFGHJKMNPQRSTUVWXYZ"
	b := make([]byte, 8)
	if _, err := rand.Read(b); err != nil {
		panic("拿不到随机数: " + err.Error()) // 系统随机源坏了，没法安全地继续
	}
	var sb strings.Builder
	for _, v := range b {
		sb.WriteByte(alphabet[int(v)%len(alphabet)])
	}
	return sb.String()
}

// ── HTTP ───────────────────────────────────────────────

type loginReq struct {
	Code string `json:"code"`
}

func (s *Server) handleLogin(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "只接受 POST", http.StatusMethodNotAllowed)
		return
	}
	var req loginReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "请求体不是合法 JSON", http.StatusBadRequest)
		return
	}
	code := strings.ToUpper(strings.TrimSpace(req.Code))

	s.access.mu.Lock()
	s.access.reload()
	u := s.access.lookup(code)
	s.access.mu.Unlock()
	if u == nil {
		time.Sleep(loginFailDelay)
		http.Error(w, "邀请码不对", http.StatusUnauthorized)
		return
	}

	http.SetCookie(w, &http.Cookie{
		Name:     cookieName,
		Value:    code,
		Path:     "/",
		HttpOnly: true, // JS 读不到，减少一条泄漏途径
		SameSite: http.SameSiteLaxMode,
		// 走隧道时是 HTTPS，Cloudflare 会带这个头。本机 HTTP 调试时不设，
		// 不然浏览器会直接丢掉这个 cookie。
		Secure: r.Header.Get("X-Forwarded-Proto") == "https",
		MaxAge: 30 * 24 * 3600,
	})
	writeJSON(w, u)
}

func (s *Server) handleMe(w http.ResponseWriter, r *http.Request) {
	_, u := s.access.From(r)
	if u == nil {
		http.Error(w, "没登录", http.StatusUnauthorized)
		return
	}
	writeJSON(w, u)
}

func (s *Server) handleLogout(w http.ResponseWriter, r *http.Request) {
	http.SetCookie(w, &http.Cookie{Name: cookieName, Path: "/", MaxAge: -1})
	w.WriteHeader(http.StatusNoContent)
}

// 包住需要登录的接口。
func (s *Server) needAuth(h func(http.ResponseWriter, *http.Request, string, *User)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		code, u := s.access.From(r)
		if u == nil {
			http.Error(w, "没登录", http.StatusUnauthorized)
			return
		}
		h(w, r, code, u)
	}
}

// access.json 的默认位置：和可执行文件的配置放一起，不在 web/ 源码目录里，
// 免得 go run 的时候被误提交。
func defaultAccessPath(dbPath string) string {
	return filepath.Join(filepath.Dir(dbPath), "access.json")
}

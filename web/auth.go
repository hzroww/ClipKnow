package main

// 登录态。Go 拥有 auth_sessions 表。
//
// ## 令牌为什么用 SHA-256 而不是 Argon2
//
// 密码那边刻意用慢哈希，因为人选的密码熵很低（八位字母数字大约 47 bit），
// 必须靠「算一次很贵」让穷举不划算。
//
// 登录令牌是**机器生成的 32 字节随机数**，256 bit 熵——穷举它在物理上不可能，
// 不需要靠慢哈希续命。而每个受保护请求都要查一次，用 Argon2 的话每次点击都
// 多花几十毫秒和 64MB 内存，纯粹是自找的。
//
// 存哈希而不是令牌本身，防的是另一件事：库被读走（备份泄漏、SQL 注入）时，
// 里面的东西不能直接拿来登录。

import (
	"crypto/rand"
	"crypto/sha256"
	"database/sql"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/google/uuid"
)

const (
	// 登录有效期。
	sessionTTL = 30 * 24 * time.Hour
	// Cookie 名。和已经删掉的邀请码那套（ck_code）不同名是有意的：
	// 老用户浏览器里残留的旧 Cookie 不会被误当成登录令牌，会被当没登录处理。
	authCookieName = "ck_session"
	// 32 字节 = 256 bit。
	tokenBytes = 32
)

var (
	ErrNoSession      = errors.New("没登录")
	ErrSessionExpired = errors.New("登录已过期")
)

// 生成令牌并落库。返回的明文只在这里出现一次——之后库里只有哈希。
func (a *Accounts) NewSession(userID string) (string, error) {
	raw := make([]byte, tokenBytes)
	if _, err := rand.Read(raw); err != nil {
		return "", err
	}
	token := base64.RawURLEncoding.EncodeToString(raw)
	now := time.Now()
	_, err := a.db.Exec(
		`INSERT INTO auth_sessions(id, user_id, token_hash, created_at, expires_at)
		 VALUES (?,?,?,?,?)`,
		uuid.NewString(), userID, hashToken(token),
		now.Unix(), now.Add(sessionTTL).Unix())
	if err != nil {
		return "", err
	}
	return token, nil
}

func hashToken(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

// 拿令牌换人。
//
// 过期、已撤销、账号被停用，一律当作「没登录」——都走同一条 401，
// 不告诉调用方具体是哪一种。
func (a *Accounts) BySessionToken(token string) (*Account, error) {
	if token == "" {
		return nil, ErrNoSession
	}
	var u Account
	var admin int
	var expires int64
	var revoked sql.NullInt64
	err := a.db.QueryRow(
		`SELECT u.id, u.username_normalized, u.display_name, u.status, u.is_admin,
		        s.expires_at, s.revoked_at
		 FROM auth_sessions s JOIN users u ON u.id = s.user_id
		 WHERE s.token_hash = ?`, hashToken(token)).
		Scan(&u.ID, &u.Username, &u.DisplayName, &u.Status, &admin, &expires, &revoked)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, ErrNoSession
	}
	if err != nil {
		return nil, err
	}
	if revoked.Valid || time.Now().Unix() >= expires {
		return nil, ErrSessionExpired
	}
	if u.Status != "active" {
		return nil, ErrUserDisabled
	}
	u.Admin = admin != 0
	return &u, nil
}

// 退出：只撤销这一条登录记录，不影响这个人在别的设备上的登录。
func (a *Accounts) RevokeSession(token string) error {
	_, err := a.db.Exec(
		`UPDATE auth_sessions SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL`,
		time.Now().Unix(), hashToken(token))
	return err
}

// 撤销某人的全部登录。改密码、停用账号时用。
func (a *Accounts) RevokeAllSessions(userID string) error {
	_, err := a.db.Exec(
		`UPDATE auth_sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL`,
		time.Now().Unix(), userID)
	return err
}

// ── Cookie ───────────────────────────────────────────────

func setAuthCookie(w http.ResponseWriter, r *http.Request, token string) {
	http.SetCookie(w, &http.Cookie{
		Name:     authCookieName,
		Value:    token,
		Path:     "/",
		HttpOnly: true, // JS 读不到，XSS 偷不走
		SameSite: http.SameSiteLaxMode,
		// 走隧道时是 HTTPS，Cloudflare 会带这个头。本机 HTTP 调试时不设，
		// 不然浏览器直接丢掉这个 cookie。
		Secure: r.Header.Get("X-Forwarded-Proto") == "https",
		MaxAge: int(sessionTTL.Seconds()),
	})
}

func clearAuthCookie(w http.ResponseWriter) {
	http.SetCookie(w, &http.Cookie{
		Name: authCookieName, Path: "/", MaxAge: -1, HttpOnly: true,
	})
}

func authTokenFrom(r *http.Request) string {
	c, err := r.Cookie(authCookieName)
	if err != nil {
		return ""
	}
	return c.Value
}

// ── 跨站请求防护 ─────────────────────────────────────────
//
// 浏览器会**自动带上 Cookie**，所以别的网站上的一段 JS 也能以你的身份向这里
// 发 POST。SameSite=Lax 挡住了大部分，但它是浏览器行为，老浏览器和某些跳转
// 场景下不生效。再查一道 Origin：跨站发来的请求带的是那个站的 Origin。
//
// 同源请求有的浏览器不带 Origin 头，所以「没有 Origin」按放行处理——
// 这一层是 SameSite 的补充，不是唯一防线。
func sameOrigin(r *http.Request) bool {
	origin := r.Header.Get("Origin")
	if origin == "" {
		return true
	}
	host := r.Header.Get("X-Forwarded-Host")
	if host == "" {
		host = r.Host
	}
	// 只比主机名部分，不比协议——反向代理后面协议经常对不上
	trim := func(s string) string {
		s = strings.TrimPrefix(strings.TrimPrefix(s, "https://"), "http://")
		return strings.SplitN(s, "/", 2)[0]
	}
	return trim(origin) == trim(host)
}

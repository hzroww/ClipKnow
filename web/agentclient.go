package main

// 调 Rust 那个常驻服务（clipknow serve）的客户端。
//
// ## 这个文件替代了什么
//
// 以前 Go 直接用只读连接查 sessions / turns / items 三张表（web/store.go）。
// 问题是**两边都得懂表结构**：Rust 写、Go 读，改一列要改两个地方，漏一个
// 就是线上 bug。现在 Go 一行聊天表的 SQL 都不写，全走 HTTP 问 Rust。
//
// Go 还留着的直接读写只有 users / auth_sessions 两张账号表——那两张是
// Go 拥有的，Rust 不碰。
//
// ## 身份怎么传
//
// 每次调用现签一张 2 分钟有效的 JWT（internaltoken.go），user_id 放在
// sub 里。**永远自己签，绝不转发浏览器传来的 Authorization 头**——转发的话
// 浏览器可以自带一个凭证绕过上面的 Cookie 校验。这里构造的是全新的
// http.Request，不复制任何入站请求头，所以这条不靠"记得"。
//
// ## 出错了给浏览器看什么
//
// 见 writeAgentError。要点是 Rust 回的 401 **不能**透传给浏览器：那意味着
// 两边密钥配错了，是服务端的问题；透传的话前端会弹「登录过期，重新输一次」，
// 把运维问题说成用户的问题。

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// 读接口的超时。
//
// 这几个查询都是亚毫秒级的，10 秒是给「Rust 那边撞上 SQLite 写锁要等满
// busy_timeout（5 秒）」留的余量。提问那条流不走这个超时——一次提问要跑
// 几分钟，见 commit 4 的 streamClient。
const agentReadTimeout = 10 * time.Second

// 提问时等**响应头**最多等多久。
//
// Rust 那边在接受请求的那一刻就发响应头（或者立刻拒绝），所以这个值只要
// 覆盖「校验归属 + 申请名额 + 起 blocking 任务」，毫秒级。给 30 秒是留给
// 撞上 SQLite 写锁要等满 busy_timeout 的情况。
//
// 拖过它说明 agent 卡住了——没有这一条的话，这个请求会永远挂着，浏览器那边
// 是一个永远不结束的加载条。
const agentHeaderTimeout = 30 * time.Second

type AgentClient struct {
	base   string // 形如 http://127.0.0.1:3100，末尾没有斜杠
	signer *tokenSigner
	read   *http.Client // 查询用，有 10 秒总超时
	stream *http.Client // 提问用，没有总超时，见 StartTurn
}

func NewAgentClient(base string, signer *tokenSigner) (*AgentClient, error) {
	u, err := url.Parse(base)
	if err != nil || u.Scheme == "" || u.Host == "" {
		return nil, fmt.Errorf("agent 地址不对: %q（该是 http://127.0.0.1:3100 这样）", base)
	}
	return &AgentClient{
		base:   strings.TrimRight(base, "/"),
		signer: signer,
		read:   &http.Client{Timeout: agentReadTimeout},
		stream: &http.Client{
			// 没有 Timeout：那是**整个请求**的上限，而 SSE 要开着几分钟。
			Transport: &http.Transport{
				ResponseHeaderTimeout: agentHeaderTimeout,
			},
		},
	}, nil
}

// Rust 那边返回的错误：{"code":"not_found","message":"没有这个会话"}。
//
// 带上 HTTP 状态码一起，因为映射给浏览器时两个都要看。
type agentError struct {
	Status  int    `json:"-"`
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *agentError) Error() string {
	return fmt.Sprintf("agent %d %s: %s", e.Status, e.Code, e.Message)
}

// 连不上 Rust。和"Rust 回了个错误"是两回事，浏览器上的提示也不一样。
type agentDownError struct{ err error }

func (e *agentDownError) Error() string { return "连不上分析服务: " + e.err.Error() }
func (e *agentDownError) Unwrap() error { return e.err }

// 发一个带凭证的 GET，把 JSON 解进 out。
func (c *AgentClient) get(userID, path string, out any) error {
	tok, err := c.signer.sign(userID)
	if err != nil {
		return err
	}
	req, err := http.NewRequest(http.MethodGet, c.base+path, nil)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+tok)

	resp, err := c.read.Do(req)
	if err != nil {
		return &agentDownError{err}
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return readAgentError(resp)
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

// 把一个非 2xx 的响应读成 agentError。
//
// 响应体不是预期的 JSON 也要给出**有信息量**的错误：那说明中间隔了个
// 代理、或者打到了别的服务上，而"解析失败"这四个字对查这类问题毫无帮助。
func readAgentError(resp *http.Response) error {
	e := &agentError{Status: resp.StatusCode}
	// 限一下读取量：万一打到了某个返回大页面的服务上，不该把它整个读进内存
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<10))
	if err := json.Unmarshal(body, e); err != nil || e.Code == "" {
		e.Code = "bad_response"
		e.Message = fmt.Sprintf("agent 返回了没法解析的 %d 响应：%s",
			resp.StatusCode, strings.TrimSpace(string(body)))
	}
	return e
}

// ── 接口 ────────────────────────────────────────────────────

// 一个会话。字段和 Rust 那边 app::sessions::SessionSummary 对应。
type Session struct {
	ID        string `json:"id"`
	Title     string `json:"title"`
	CreatedAt int64  `json:"created_at"`
}

// 聊天记录里的一条。对应 app::sessions::UiMessage。
type Message struct {
	Role   string `json:"role"` // "user" | "assistant"
	Text   string `json:"text"`
	Seq    int64  `json:"seq"`
	Failed bool   `json:"failed"` // 这一轮是失败收场的
}

// 这个用户的会话，最近有活动的排前面。
func (c *AgentClient) Sessions(userID string, limit int) ([]Session, error) {
	var out struct {
		Sessions []Session `json:"sessions"`
	}
	if err := c.get(userID, fmt.Sprintf("/internal/sessions?limit=%d", limit), &out); err != nil {
		return nil, err
	}
	// 空切片而不是 nil：nil 会被 encoding/json 序列化成 null，
	// 前端 .map() 直接报错。
	if out.Sessions == nil {
		out.Sessions = []Session{}
	}
	return out.Sessions, nil
}

// 一个会话的聊天记录。不是自己的 / 不存在 / 已删，都返回一个 404 的 agentError。
func (c *AgentClient) Messages(userID, sessionID string) ([]Message, error) {
	var out struct {
		Messages []Message `json:"messages"`
	}
	path := "/internal/sessions/" + url.PathEscape(sessionID) + "/messages"
	if err := c.get(userID, path, &out); err != nil {
		return nil, err
	}
	if out.Messages == nil {
		out.Messages = []Message{}
	}
	return out.Messages, nil
}

// 新建一个属于这个用户的空会话，返回会话 id。
//
// ★ 归属在**创建那一刻**就写进库，不是事后认领。改造前是 Rust 子进程先建、
//
//	把 id 放在 hello 那一行报回来、Go 再认领一次——中间那一小段时间里会话
//	是无主的，认领失败（进程被杀）就永远无主了。
func (c *AgentClient) CreateSession(userID string) (string, error) {
	tok, err := c.signer.sign(userID)
	if err != nil {
		return "", err
	}
	req, err := http.NewRequest(http.MethodPost, c.base+"/internal/sessions",
		strings.NewReader(`{}`))
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+tok)
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.read.Do(req)
	if err != nil {
		return "", &agentDownError{err}
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusCreated {
		return "", readAgentError(resp)
	}
	var out struct {
		SessionID string `json:"session_id"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return "", err
	}
	if out.SessionID == "" {
		return "", fmt.Errorf("agent 建了会话但没回 id")
	}
	return out.SessionID, nil
}

// 提问。返回一个**还没读完**的响应，调用方负责把 body 读完并关掉。
//
// ## 超时怎么设
//
// 这条**不能**用 c.read 那个客户端——它有 10 秒总超时，而一次提问要跑几十秒
// 到几分钟。这里用一个没有总超时的客户端，但设了 ResponseHeaderTimeout：
//
//	总超时（Timeout）        不设。设了就是给"一次提问最多跑多久"划线，
//	                        而那条线该由 Rust 那边的闸门管，不是这里。
//	ResponseHeaderTimeout   30 秒。Rust 在接受请求时就会发出响应头
//	                        （或者立刻拒绝），拖过 30 秒说明它卡住了。
//	                        没有这一条的话，agent 半死时这个请求会永远挂着。
func (c *AgentClient) StartTurn(userID, sessionID, question, provider string) (*http.Response, error) {
	tok, err := c.signer.sign(userID)
	if err != nil {
		return nil, err
	}
	// ★ provider 为空时**整个字段不传**，不传一个空字符串。
	//
	//   传空串的话，Rust 那边收到的是 Some("")，而它的白名单里没有空串，
	//   于是一个「没指定模型」的正常请求变成 400「不认识的 provider: 」。
	//   （和 docker-compose 那个 ${VAR:-} 传空串的坑是同一类：
	//     "没给" 和 "给了个空的" 必须是两回事。）
	payload := map[string]string{"question": question}
	if strings.TrimSpace(provider) != "" {
		payload["provider"] = provider
	}
	body, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	url := c.base + "/internal/sessions/" + url.PathEscape(sessionID) + "/turns"
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+tok)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "text/event-stream")

	resp, err := c.stream.Do(req)
	if err != nil {
		return nil, &agentDownError{err}
	}
	if resp.StatusCode != http.StatusOK {
		defer resp.Body.Close()
		// 流还没开始，拒绝走 HTTP 状态码——400 参数错 / 404 不是你的会话 /
		// 409 这个会话在跑 / 503 名额满。
		return nil, readAgentError(resp)
	}
	return resp, nil
}

// ── 启动自检 ────────────────────────────────────────────────

// 起服务前确认 Rust 那边真的能用。
//
// 分两步，因为它们查的是两件不同的事：
//
//	health   端口通不通、库能不能读（不需要凭证，所以密钥配错它也是通的）
//	whoami   两边密钥是不是同一个
//
// 只探 health 的话，密钥配错要等到用户提第一个问题才报 401——那时候人已经
// 在等答案了，而错误信息是「登录过期」，指向完全错误的方向。
func (c *AgentClient) Probe() error {
	var h struct {
		OK            bool  `json:"ok"`
		SchemaVersion int64 `json:"schema_version"`
	}
	// health 不设防，用 get 会白签一张凭证，但省一套代码，无所谓
	if err := c.get("probe", "/internal/health", &h); err != nil {
		return fmt.Errorf("agent 探活失败（%s）：%w\n"+
			"先在另一个终端里跑：clipknow serve --db <库>", c.base, err)
	}
	if !h.OK {
		return fmt.Errorf("agent 说自己不健康：%+v", h)
	}

	var who struct {
		UserID string `json:"user_id"`
	}
	const probeUser = "probe-user"
	if err := c.get(probeUser, "/internal/whoami", &who); err != nil {
		return fmt.Errorf("agent 不认我签的凭证：%w\n"+
			"多半是 %s 两边配得不一样", err, internalSecretEnv)
	}
	if who.UserID != probeUser {
		return fmt.Errorf("agent 回的身份不对：期望 %q，实际 %q", probeUser, who.UserID)
	}
	log.Printf("  agent %s（库版本 %d）", c.base, h.SchemaVersion)
	return nil
}

// ── 错误怎么翻译给浏览器 ────────────────────────────────────

// 把调用 agent 时的错误变成给浏览器的 HTTP 响应。
//
// 映射表（左边是 Rust 回的，右边是浏览器看到的）：
//
//	连不上        → 503  "分析服务没在跑"
//	400 参数错    → 400  原样
//	401 凭证不对  → 502  **不透传**。401 在浏览器那边的含义是"你的登录失效了"，
//	                     而这里的 401 是"Go 和 Rust 的密钥配得不一样"——
//	                     一个运维问题。透传会让用户去重新登录，白折腾。
//	404 没找到    → 404  原样
//	409 会话忙    → 409  原样
//	429/503 容量  → 503  原样
//	500 内部错    → 502  上游炸了，不是本服务炸了
func writeAgentError(w http.ResponseWriter, err error) {
	var down *agentDownError
	if errors.As(err, &down) {
		log.Printf("agent 连不上: %v", down.err)
		http.Error(w, "分析服务没在跑，联系管理员", http.StatusServiceUnavailable)
		return
	}

	var ae *agentError
	if !errors.As(err, &ae) {
		log.Printf("调 agent 出错: %v", err)
		http.Error(w, "内部错误", http.StatusInternalServerError)
		return
	}

	switch ae.Status {
	case http.StatusBadRequest, http.StatusNotFound, http.StatusConflict:
		http.Error(w, ae.Message, ae.Status)
	case http.StatusTooManyRequests, http.StatusServiceUnavailable:
		http.Error(w, ae.Message, http.StatusServiceUnavailable)
	case http.StatusUnauthorized:
		// 见上面映射表里这一行的说明。原因只进服务端日志。
		log.Printf("★ agent 拒绝了我的凭证，检查两边的 %s 是不是同一个：%v",
			internalSecretEnv, ae)
		http.Error(w, "分析服务配置有问题，联系管理员", http.StatusBadGateway)
	default:
		log.Printf("agent 返回 %d: %v", ae.Status, ae)
		http.Error(w, "分析服务出错了", http.StatusBadGateway)
	}
}

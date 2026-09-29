package main

// Go → Rust 的身份凭证：签发这一半。校验那一半在 src/serve/auth.rs。
//
// ## 为什么需要它
//
// Go 管账号（注册、Argon2id 校验密码、ck_session Cookie），Rust 管会话和
// 执行。Rust 完全不碰浏览器 Cookie，所以需要一个东西让它相信「这个请求
// 确实代表 users 表里的某一行」。
//
// 做法：Go 每转发一次请求就现签一个 2 分钟有效的 JWT，里面只放 user_id。
//
// ## 三条规矩
//
//  1. **浏览器传来的 Authorization 头一律丢掉，永远自己重新签。**
//     不丢的话，浏览器可以自带一个凭证绕过上面的 Cookie 校验——那等于
//     整套登录形同虚设。这条在 agentclient.go 里执行（它构造的是全新的
//     http.Request，不复制任何入站请求头）。
//
//  2. **user_id 只能来自 needAuth 验过的 Account，不能来自请求体。**
//     浏览器传什么 user_id 都不看。
//
//  3. **密钥两边必须一致，没配就拒绝启动。**
//     不存在「没配就不校验」的降级路径。
//
// ## 有效期为什么只有 2 分钟
//
// 凭证只在 Rust **接受请求**的那一刻校验一次；已经开跑的执行不会因为
// 凭证过期而中断（一次提问可能跑几分钟）。所以有效期只需要覆盖「Go 签完
// 到 Rust 收到」这段路，本机上是微秒级。给 2 分钟纯粹是留给时钟漂移。

import (
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/golang-jwt/jwt/v5"
)

const (
	// 密钥从这个环境变量读。Rust 那边读的是同一个名字（src/serve/auth.rs）。
	internalSecretEnv = "CLIPKNOW_INTERNAL_SECRET"
	// 谁签的 / 给谁的。两边都校验，是为了让这个密钥只能用来签这一种凭证。
	internalIssuer   = "clipknow-web"
	internalAudience = "clipknow-agent"
	// HS256 的安全性全压在密钥的熵上：抓到一个凭证就能离线暴力破解密钥，
	// 而拿到密钥等于能冒充任意用户。32 字节是 `openssl rand -base64 32`
	// 随手就能满足的量，挡的是「图省事写了个 dev」。
	minInternalSecretLen = 32
	// 见文件头「有效期为什么只有 2 分钟」。
	internalTokenTTL = 2 * time.Minute
)

type tokenSigner struct{ secret []byte }

// 从环境变量建一个签发器。没配或太短都报错，调用方应该据此拒绝启动。
func newTokenSigner() (*tokenSigner, error) {
	// os.Getenv 对「设成空串」和「没设」返回同样的结果，这里不用区分：
	// 两种都是没配。（Rust 那边同理，见 clipknow::env_var 的注释。）
	s := strings.TrimSpace(os.Getenv(internalSecretEnv))
	if s == "" {
		return nil, fmt.Errorf(
			"没有配 %s。\n"+
				"生成一个：openssl rand -base64 32\n"+
				"Go 和 Rust 两边必须配成同一个值",
			internalSecretEnv)
	}
	if len(s) < minInternalSecretLen {
		return nil, fmt.Errorf(
			"%s 太短（%d 字符，至少要 %d）。\n"+
				"生成一个：openssl rand -base64 32",
			internalSecretEnv, len(s), minInternalSecretLen)
	}
	return &tokenSigner{secret: []byte(s)}, nil
}

// 给一个用户签一张凭证。
func (s *tokenSigner) sign(userID string) (string, error) {
	return s.signAt(userID, time.Now())
}

// 指定"现在"是几点来签。
//
// 拆出这个参数只为一件事：让跨语言的黄金样例测试能产出**确定的**字符串。
// 时间一变签名就变，那条测试就没法写成定值比对了。
func (s *tokenSigner) signAt(userID string, now time.Time) (string, error) {
	if strings.TrimSpace(userID) == "" {
		// 空的 user_id 会让 Rust 那边的归属查询匹配到 0 行，表现成「一条
		// 会话都没有」——一个很难查的静默失败。在源头拦住。
		return "", fmt.Errorf("user_id 是空的，不能签凭证")
	}
	// ★ 用 MapClaims 而不是 jwt.RegisteredClaims：后者的 aud 字段类型是
	//   ClaimStrings，序列化出来是**数组** ["clipknow-agent"]。Rust 那边
	//   两种形状都认，但形状是协议的一部分，写成确定的单个字符串少一个
	//   将来会踩的坑。
	//
	//   MapClaims 底下是 map[string]any，encoding/json 序列化 map 时按键名
	//   排序，所以同样的输入永远产出同样的字节——黄金样例才成立。
	claims := jwt.MapClaims{
		"sub": userID,
		"iss": internalIssuer,
		"aud": internalAudience,
		"iat": now.Unix(),
		"exp": now.Add(internalTokenTTL).Unix(),
	}
	// SigningMethodHS256 必须和 Rust 那边 Validation::new(Algorithm::HS256)
	// 写死的算法一致。改这里就要同时改那边，两条测试会喊。
	return jwt.NewWithClaims(jwt.SigningMethodHS256, claims).SignedString(s.secret)
}

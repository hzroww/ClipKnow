package main

import (
	"encoding/base64"
	"encoding/json"
	"strings"
	"testing"
	"time"
)

// ★ 跨语言的黄金样例。
//
// 这一串是 signAt 用固定密钥、固定时间戳产出的。src/serve/auth.rs 里的
// `go_签出来的凭证_rust_认得` 那条测试用同样的密钥去验它。
//
// 两条测试合起来钉住的是「Go 签的东西 Rust 认得」。少了这个，Go 这边改个
// 字段名（比如把 sub 写成 user_id）、换个签名算法，两边各自的单元测试都
// 还是绿的，要等端到端测试才炸——而端到端跑得慢、报错离现场远。
//
// 这条红了的话，**先想清楚是不是真要改协议**：要改就两边一起改，
// 重新生成这个值并同步到 auth.rs。
const goldenSecret = "golden-secret-for-cross-language-test"
const goldenToken = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9." +
	"eyJhdWQiOiJjbGlwa25vdy1hZ2VudCIsImV4cCI6MTcwMDAwMDEyMCwiaWF0IjoxNzAwMDAwMDAwLCJpc3MiOiJjbGlwa25vdy13ZWIiLCJzdWIiOiJ1X2dvbGRlbiJ9." +
	"OjKLCydFro1aP4_fcxNqRdivjYdWQu36Af7XxBaeB8M"

func TestGoldenTokenIsStable(t *testing.T) {
	s := &tokenSigner{secret: []byte(goldenSecret)}
	got, err := s.signAt("u_golden", time.Unix(1700000000, 0))
	if err != nil {
		t.Fatal(err)
	}
	if got != goldenToken {
		t.Fatalf("签出来的凭证和黄金样例不一样，说明协议变了。\n"+
			"要是这个改动是故意的，把新值同步到 src/serve/auth.rs 的 GOLDEN_TOKEN。\n"+
			"实际 %s\n期望 %s", got, goldenToken)
	}
}

// 字段名是协议的一部分：Rust 那边按这些名字解析。
// 上面那条定值比对其实已经覆盖了，但它红的时候只会说"不一样"；
// 这条能直接指出是哪个字段出了问题。
func TestClaimNamesMatchWhatRustExpects(t *testing.T) {
	s := &tokenSigner{secret: []byte(goldenSecret)}
	tok, err := s.signAt("u_abc", time.Now())
	if err != nil {
		t.Fatal(err)
	}
	parts := strings.Split(tok, ".")
	if len(parts) != 3 {
		t.Fatalf("JWT 应该是三段，实际 %d 段", len(parts))
	}
	raw, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	var c map[string]any
	if err := json.Unmarshal(raw, &c); err != nil {
		t.Fatal(err)
	}
	if c["sub"] != "u_abc" {
		t.Errorf("sub 应该是 user_id，实际 %v", c["sub"])
	}
	if c["iss"] != internalIssuer {
		t.Errorf("iss = %v，期望 %s", c["iss"], internalIssuer)
	}
	// ★ aud 必须是**单个字符串**，不是数组。
	//   jwt.RegisteredClaims 的 aud 类型会序列化成 ["clipknow-agent"]，
	//   换回 RegisteredClaims 的话这条会红。
	if c["aud"] != internalAudience {
		t.Errorf("aud = %#v，期望单个字符串 %q（不是数组）", c["aud"], internalAudience)
	}
	if _, ok := c["exp"]; !ok {
		t.Error("没有 exp —— 那就是一张永久有效的凭证，Rust 那边会拒")
	}

	// 头部的算法也要对上 Rust 写死的 HS256
	rawHdr, err := base64.RawURLEncoding.DecodeString(parts[0])
	if err != nil {
		t.Fatal(err)
	}
	var h map[string]any
	if err := json.Unmarshal(rawHdr, &h); err != nil {
		t.Fatal(err)
	}
	if h["alg"] != "HS256" {
		t.Errorf("alg = %v，Rust 那边只接受 HS256", h["alg"])
	}
}

func TestExpiryIsShort(t *testing.T) {
	s := &tokenSigner{secret: []byte(goldenSecret)}
	base := time.Unix(1700000000, 0)
	tok, err := s.signAt("u_abc", base)
	if err != nil {
		t.Fatal(err)
	}
	parts := strings.Split(tok, ".")
	raw, _ := base64.RawURLEncoding.DecodeString(parts[1])
	// map[string]any 而不是 map[string]float64：aud 和 sub 是字符串，
	// 用数字类型解会整个解析失败（第一版就栽在这）。
	var c map[string]any
	if err := json.Unmarshal(raw, &c); err != nil {
		t.Fatal(err)
	}
	exp, ok1 := c["exp"].(float64)
	iat, ok2 := c["iat"].(float64)
	if !ok1 || !ok2 {
		t.Fatalf("exp/iat 不是数字：exp=%#v iat=%#v", c["exp"], c["iat"])
	}
	ttl := time.Duration(exp-iat) * time.Second
	// 上限是安全要求：凭证漏出去之后的可用窗口。
	// 下限是防手滑写成几秒——那样时钟稍微漂一点就全线 401。
	if ttl > 5*time.Minute || ttl < time.Minute {
		t.Errorf("有效期 %v 不在 1~5 分钟之间", ttl)
	}
}

func TestEmptyUserIDIsRefused(t *testing.T) {
	s := &tokenSigner{secret: []byte(goldenSecret)}
	for _, id := range []string{"", "   ", "\t"} {
		if _, err := s.signAt(id, time.Now()); err == nil {
			t.Errorf("user_id=%q 居然签出来了。空的 user_id 会让 Rust 那边"+
				"查到 0 行会话，表现成「一条会话都没有」，很难查", id)
		}
	}
}

func TestSignerRefusesMissingOrWeakSecret(t *testing.T) {
	cases := []struct {
		name, value string
		wantIn      string
	}{
		{"没设", "", "没有配"},
		{"只有空白", "   ", "没有配"},
		{"太短", "dev", "太短"},
		{"差一个字符", strings.Repeat("a", minInternalSecretLen-1), "太短"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			t.Setenv(internalSecretEnv, c.value)
			_, err := newTokenSigner()
			if err == nil {
				t.Fatal("居然让它过了。没配密钥就不校验是最典型的上线事故")
			}
			if !strings.Contains(err.Error(), c.wantIn) {
				t.Errorf("报错里没有 %q：%v", c.wantIn, err)
			}
			// 报错要带上补救办法
			if !strings.Contains(err.Error(), "openssl rand") {
				t.Errorf("报错里没告诉人怎么生成一个：%v", err)
			}
		})
	}
}

func TestSignerAcceptsExactlyMinimumLength(t *testing.T) {
	t.Setenv(internalSecretEnv, strings.Repeat("a", minInternalSecretLen))
	if _, err := newTokenSigner(); err != nil {
		t.Fatalf("刚好够长度的密钥被拒了：%v", err)
	}
}

// 密钥不一样，签出来的凭证就不一样。
// 这条防的是「密钥根本没参与签名」这种低级但致命的写法。
func TestDifferentSecretsProduceDifferentTokens(t *testing.T) {
	a := &tokenSigner{secret: []byte(strings.Repeat("a", 40))}
	b := &tokenSigner{secret: []byte(strings.Repeat("b", 40))}
	at, _ := a.signAt("u_abc", time.Unix(1700000000, 0))
	bt, _ := b.signAt("u_abc", time.Unix(1700000000, 0))
	if at == bt {
		t.Fatal("换了密钥签出来一模一样，说明密钥没参与签名")
	}
}

package main

import (
	"strings"
	"testing"
)

func TestHashAndVerify(t *testing.T) {
	const pw = "正确的马电池订书钉"
	h, err := hashPassword(pw)
	if err != nil {
		t.Fatal(err)
	}
	if !verifyPassword(pw, h) {
		t.Error("正确的密码必须验过")
	}
	if verifyPassword(pw+"x", h) {
		t.Error("错的密码必须验不过")
	}
	if verifyPassword("", h) {
		t.Error("空密码必须验不过")
	}
}

func TestHashIsSelfDescribing(t *testing.T) {
	h, err := hashPassword("x")
	if err != nil {
		t.Fatal(err)
	}
	// 算法、版本、参数、salt、哈希五段都要在，以后调参数才不会把老用户锁在门外
	for _, want := range []string{"$argon2id$", "v=19", "m=65536", "t=3", "p=2"} {
		if !strings.Contains(h, want) {
			t.Errorf("哈希串里少了 %q：%s", want, h)
		}
	}
	if n := len(strings.Split(h, "$")); n != 6 {
		t.Errorf("哈希串应该是 5 段（6 个 $ 分片），实际 %d：%s", n, h)
	}
}

func TestSametPasswordHashesDifferently(t *testing.T) {
	// salt 随机，所以同一个密码两次哈希必须不同。
	// 相同的话说明 salt 没生效——一次撞库就能同时打穿所有用同一密码的账号。
	a, _ := hashPassword("same")
	b, _ := hashPassword("same")
	if a == b {
		t.Fatal("同一个密码两次哈希不该相同，说明 salt 没起作用")
	}
	if !verifyPassword("same", a) || !verifyPassword("same", b) {
		t.Error("两个都该能验过")
	}
}

func TestVerifyRejectsGarbage(t *testing.T) {
	// 库里的值被改坏、或者是别的算法的串，都必须是「验不过」而不是 panic
	for _, bad := range []string{
		"", "不是哈希", "$argon2id$", "$bcrypt$v=19$m=1,t=1,p=1$YQ$Yg",
		"$argon2id$v=99$m=1,t=1,p=1$YQ$Yg", // 版本不对
		"$argon2id$v=19$乱七八糟$YQ$Yg",
		"$argon2id$v=19$m=1,t=1,p=1$!!!$Yg", // salt 不是合法 base64
	} {
		if verifyPassword("whatever", bad) {
			t.Errorf("坏串 %q 不该验过", bad)
		}
	}
}

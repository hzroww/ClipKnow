package main

// 密码哈希。用 Argon2id。
//
// ## 为什么不是 SHA-256 之类
//
// 普通哈希是**为了快**设计的，而密码哈希要的恰恰相反：慢到让逐个猜的人算不动。
// 现代显卡一秒能算几十亿次 SHA-256，一个八位密码几分钟就穷举完了。
// Argon2id 可以调「一次要花多少内存、多少轮」，把这个数字压到每秒几十次。
//
// Argon2id 是 2015 年密码哈希竞赛的冠军，也是 OWASP 现在的首选。
// 用 golang.org/x/crypto 的实现，不自己写密码学代码。
//
// ## 存的是什么
//
// 不是裸哈希，是一整串自描述的编码：
//
//	$argon2id$v=19$m=65536,t=3,p=2$<salt base64>$<hash base64>
//
// 把算法、参数和 salt 都写在里面。好处是**以后调参数不会把老用户锁在门外**：
// 验证时按串里记的旧参数算，对上就放行，顺手可以用新参数重算一遍存回去。
// 只存裸哈希的话，参数一改，所有人都登不进来。

import (
	"crypto/rand"
	"crypto/subtle"
	"encoding/base64"
	"errors"
	"fmt"
	"strings"

	"golang.org/x/crypto/argon2"
)

// 参数。OWASP 2024 年给 Argon2id 的建议档位之一（64MB / 3 轮 / 并行 2）。
//
// 这几个数是**按部署资源校准**的，不是越大越好：每次登录都要真的分配 64MB
// 内存算一遍，同时来 100 个登录就是 6.4GB。当前场景是几个人用一台 Mac，
// 64MB 完全撑得住；真要扛并发登录时再往下调，老密码串里记着旧参数，不会锁人。
const (
	argonMemoryKiB  = 64 * 1024 // 64 MB
	argonIterations = 3
	argonParallel   = 2
	argonSaltLen    = 16
	argonKeyLen     = 32
)

var errBadHashFormat = errors.New("密码哈希串格式不对")

// 把明文密码变成可以入库的编码串。
func hashPassword(plain string) (string, error) {
	salt := make([]byte, argonSaltLen)
	if _, err := rand.Read(salt); err != nil {
		// 系统随机源坏了，没法安全地继续
		return "", fmt.Errorf("拿不到随机数: %w", err)
	}
	key := argon2.IDKey([]byte(plain), salt, argonIterations, argonMemoryKiB, argonParallel, argonKeyLen)
	return fmt.Sprintf(
		"$argon2id$v=%d$m=%d,t=%d,p=%d$%s$%s",
		argon2.Version, argonMemoryKiB, argonIterations, argonParallel,
		base64.RawStdEncoding.EncodeToString(salt),
		base64.RawStdEncoding.EncodeToString(key),
	), nil
}

// 验证密码。
//
// ★ 用 subtle.ConstantTimeCompare 而不是 ==。
//
//	普通的字节比较一发现不同就返回，快慢泄漏了「前几个字节对了」这个信息，
//	理论上能被逐字节试出来。这里几乎不花钱，就做了。
func verifyPassword(plain, encoded string) bool {
	mem, iter, par, salt, want, err := parseHash(encoded)
	if err != nil {
		return false
	}
	got := argon2.IDKey([]byte(plain), salt, iter, mem, par, uint32(len(want)))
	return subtle.ConstantTimeCompare(got, want) == 1
}

// 拆开编码串。按串里记的参数算，不是按当前常量算——这就是老用户不会被锁在
// 门外的原因。
func parseHash(encoded string) (mem, iter uint32, par uint8, salt, key []byte, err error) {
	parts := strings.Split(encoded, "$")
	// ["", "argon2id", "v=19", "m=..,t=..,p=..", salt, key]
	if len(parts) != 6 || parts[1] != "argon2id" {
		return 0, 0, 0, nil, nil, errBadHashFormat
	}
	var version int
	if _, err = fmt.Sscanf(parts[2], "v=%d", &version); err != nil || version != argon2.Version {
		return 0, 0, 0, nil, nil, errBadHashFormat
	}
	if _, err = fmt.Sscanf(parts[3], "m=%d,t=%d,p=%d", &mem, &iter, &par); err != nil {
		return 0, 0, 0, nil, nil, errBadHashFormat
	}
	if salt, err = base64.RawStdEncoding.DecodeString(parts[4]); err != nil {
		return 0, 0, 0, nil, nil, errBadHashFormat
	}
	if key, err = base64.RawStdEncoding.DecodeString(parts[5]); err != nil {
		return 0, 0, 0, nil, nil, errBadHashFormat
	}
	return mem, iter, par, salt, key, nil
}

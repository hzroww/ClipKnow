package main

// 测试里怎么把 Rust 那个常驻服务拉起来。
//
// 以前测试只需要一个二进制文件（每次提问 exec 一下就完了）。现在 Rust 是
// 常驻的，测试得**真的起一个进程**、等它听上端口、结束时收干净。
//
// 三件事值得说明：
//
//  1. 端口给 `127.0.0.1:0` 让系统挑一个空闲的，然后**从它的 stderr 里把
//     实际端口读回来**。写死端口的话，并行跑的测试会互相撞；Go 这边先
//     bind 一个再关掉、把端口号传过去，中间那一瞬间照样可能被别人抢走。
//
//  2. 起不来的时候，把它的 stderr 原样打出来。缺密钥、库没迁移、端口被占，
//     原因全在那几行里；只报一句"20 秒没起来"等于把线索扔了。
//
//  3. 收尾发 SIGTERM 而不是 Kill：那是 clipknow serve 优雅停机的入口。
//     用 Kill 的话，"停机写坏了"这件事在测试里永远暴露不出来。

import (
	"bufio"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

// 测试用的内部凭证密钥。必须 >= 32 字符，两边（Go 客户端和 Rust 进程）用同一个。
const testInternalSecret = "clipknow-test-internal-secret-32ch"

// serve 起来之后 stderr 上那一行：`ClipKnow agent 服务  →  http://127.0.0.1:53421`
var agentAddrLine = regexp.MustCompile(`http://127\.0\.0\.1:\d+`)

// 等 serve 报出地址最多等多久。正常是几十毫秒；给这么宽是因为 CI 的机器
// 可能很慢，而这个超时只在**失败**路径上真的等满。
const agentStartTimeout = 20 * time.Second

// 起一个真的 clipknow serve 子进程，返回指向它的客户端。
// 测试结束时自动发 SIGTERM 收掉。
func startAgent(t *testing.T, bin, db string) *AgentClient {
	t.Helper()
	t.Setenv(internalSecretEnv, testInternalSecret)

	cmd := exec.Command(bin, "serve", "--db", db, "--addr", "127.0.0.1:0")
	// ★ 钉住工作目录，不继承测试进程的。
	//   Rust 那边 dotenvy 从当前目录**往上级找** .env。不钉的话，测试会
	//   捞到项目根目录那个装着真 key 的 .env——一个本该离线的测试就可能
	//   花掉真钱。临时目录里没有 .env，所以这里是安全的。
	cmd.Dir = filepath.Dir(db)

	stderr, err := cmd.StderrPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := cmd.Start(); err != nil {
		t.Fatalf("起不了 clipknow serve: %v", err)
	}
	t.Cleanup(func() {
		_ = cmd.Process.Signal(syscall.SIGTERM)
		_ = cmd.Wait()
	})

	addrCh := make(chan string, 1)
	var mu sync.Mutex
	var logs []string
	go func() {
		sc := bufio.NewScanner(stderr)
		for sc.Scan() {
			line := sc.Text()
			mu.Lock()
			logs = append(logs, line)
			mu.Unlock()
			if m := agentAddrLine.FindString(line); m != "" {
				select {
				case addrCh <- m:
				default: // 已经报过一次了
				}
			}
		}
	}()

	select {
	case base := <-addrCh:
		c, err := NewAgentClient(base, &tokenSigner{secret: []byte(testInternalSecret)})
		if err != nil {
			t.Fatal(err)
		}
		// 报出地址只说明 listener 起来了。再探一次 health + whoami，
		// 确认库能读、密钥也对得上——和 main() 启动时做的是同一件事。
		if err := c.Probe(); err != nil {
			t.Fatalf("agent 起来了但自检没过: %v", err)
		}
		return c
	case <-time.After(agentStartTimeout):
		mu.Lock()
		why := strings.Join(logs, "\n")
		mu.Unlock()
		_ = cmd.Process.Kill()
		t.Fatalf("clipknow serve %v 内没报出监听地址。它的 stderr：\n%s",
			agentStartTimeout, why)
		return nil
	}
}

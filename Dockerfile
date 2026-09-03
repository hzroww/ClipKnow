# ClipKnow 的镜像：两个语言各编一遍，最后只留两个二进制。
#
# 为什么运行镜像可以这么薄（只加了根证书）：
#   rusqlite 用 bundled 特性     —— SQLite 从源码编进二进制，不依赖系统 libsqlite3
#   reqwest 链的是 rustls        —— 不依赖 OpenSSL（Cargo.lock 里没有 openssl-sys）
#   迁移用 include_str!          —— .sql 编进二进制，运行时不读文件
#   前端用 go:embed              —— index.html 编进 Go 二进制
#   modernc.org/sqlite 是纯 Go   —— CGO_ENABLED=0 能静态编
# 所以运行时唯一的外部依赖是「能校验 HTTPS 证书」。

# ── ① Rust 核心 ──────────────────────────────────────────
FROM rust:1-slim AS rust-build
WORKDIR /src
# rusqlite 的 bundled 要现编 SQLite 的 C 代码，slim 里没有编译器
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*
# 先只拷清单去拉依赖：改代码不重新下载 crates。
# cargo fetch 要求 target 文件存在（不然报 no targets specified），所以先放占位，
# 下面 COPY src 会盖掉它们。fetch 只下载不编译，没有旧产物残留的问题。
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main(){}' > src/main.rs && touch src/lib.rs
RUN cargo fetch --locked
COPY src ./src
COPY migrations ./migrations
# --bin clipknow：不编 examples（那些是探针，会联网花钱）
RUN cargo build --release --locked --bin clipknow

# ── ② Go web 层 ─────────────────────────────────────────
FROM golang:1.27-alpine AS go-build
WORKDIR /src/web
COPY web/go.mod web/go.sum ./
RUN go mod download
COPY web/ ./
# CGO_ENABLED=0：SQLite 驱动是纯 Go 的，静态编出来不依赖 libc
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/clipknow-web .

# ── ③ 运行 ──────────────────────────────────────────────
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -u 10001 clipknow
COPY --from=rust-build /src/target/release/clipknow      /usr/local/bin/clipknow
COPY --from=go-build   /out/clipknow-web                 /usr/local/bin/clipknow-web

# 库和邀请码都在这里。access.json 的路径是「库所在目录 + access.json」，
# 所以挂一个卷两样都覆盖到。
# ★ 必须在 USER 之前把 /data 建好并改所有者。
#   Docker 给**空的命名卷**做初始化时会照抄镜像里这个目录的所有者，所以镜像里
#   建对了，卷就是对的。不这么做的话容器以 uid 10001 跑，写 /data/access.json
#   直接 permission denied——实测过，容器起来就退。
#   （挂 bind mount 的话这招不管用，宿主目录得自己 chown 10001。）
RUN mkdir -p /data && chown clipknow:clipknow /data
VOLUME /data
USER clipknow
EXPOSE 3000

# -bin 指向 Rust 二进制：web 每次提问起一个子进程
CMD ["/usr/local/bin/clipknow-web", \
     "-addr", ":3000", \
     "-db",   "/data/clipknow.db", \
     "-bin",  "/usr/local/bin/clipknow"]

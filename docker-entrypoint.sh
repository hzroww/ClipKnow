#!/bin/sh
# 容器启动：先把库升级到位，再起服务。
#
# 为什么放在这里而不是让服务自己做：设计文档要求「服务启动只检查版本，不各自
# 竞争执行 DDL」——因为 Go 和 Rust 是两个会同时启动的进程。入口脚本是**一个**
# 串行的地方，跑完了才拉起服务，不存在竞争。
#
# migrate 本身幂等：已经是最新版就什么都不做。
set -e

echo "== 检查数据库版本 =="
clipknow migrate --db "${CLIPKNOW_DB:-/data/clipknow.db}"

echo "== 启动 web =="
exec "$@"

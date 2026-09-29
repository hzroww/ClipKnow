#!/bin/sh
# 容器启动。
#
# 迁移**只在设了 CLIPKNOW_MIGRATE=1 的那个服务里跑**（compose 里只有 agent
# 设了）。两个服务都跑的话，就是两个进程同时对同一个 SQLite 文件执行建表
# 语句——设计文档明确要求「服务启动只检查版本，不各自竞争执行 DDL」。
#
# web 那个容器靠 compose 的 depends_on: service_healthy 等 agent 做完，
# 所以它启动时库一定已经是最新版了。
#
# migrate 本身幂等：已经是最新版就什么都不做。
set -e

if [ "${CLIPKNOW_MIGRATE:-0}" = "1" ]; then
  echo "== 检查数据库版本 =="
  clipknow migrate --db "${CLIPKNOW_DB:-/data/clipknow.db}"
fi

exec "$@"

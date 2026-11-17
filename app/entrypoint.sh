#!/bin/sh
# /data の所有者を合わせてから、非 root ユーザーで起動する
set -e
if [ "$(id -u)" = "0" ]; then
  chown -R app:app /data
  exec su-exec app "$@"
fi
exec "$@"

#!/bin/sh
# yt-upgrade — 每日包升级 + 清缓存（systemd timer 驱动）
#
# 取代 CT411 上 upgrade-containers.sh 对这台的集中升级：容器自带 timer，
# CT411 停机也照常升级。
#
# 只升级、不重启服务；需要新内核/libc 生效要等容器重启。
# 与 apt-daily-upgrade.timer（只升安全更新）并存，各自幂等。

LOG=/var/log/yt-upgrade.log

{
    echo "=== $(date '+%F %T') start"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq 2>&1 | tail -2
    before=$(apt list --upgradable 2>/dev/null | tail -n +2 | wc -l)
    apt-get -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold" full-upgrade 2>&1 | tail -8
    after=$(apt list --upgradable 2>/dev/null | tail -n +2 | wc -l)
    apt-get clean
    rm -rf /var/cache/apt/archives/*.deb
    echo "=== $(date '+%F %T') done before=$before after=$after"
} >>"$LOG" 2>&1

tail -n 500 "$LOG" > "$LOG.tmp" 2>/dev/null && mv "$LOG.tmp" "$LOG"

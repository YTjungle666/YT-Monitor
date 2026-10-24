#!/bin/bash
# 对没有 init 的容器保证 ytmon-agent 存活。
#
# 为什么不在容器里做：这些容器的 PID1 就是应用本身（start.sh / AdGuardHome /
# node / apache2 / yt-entrypoint.sh / container-init），没有 rc、没有 systemd，
# 新建 unit 无人执行。改 vendor 的 entrypoint 会在产品升级时被覆盖，风险高于收益。
#
# 代价：容器重启后最长 3 分钟的空窗。对健康监控可接受；要消除它得改 entrypoint。
#
# 由一台常驻主机的 cron 每 3 分钟调用：*/3 * * * * <repo>/scripts/watchdog-noinit.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# cp deploy/noinit-hosts.example.txt deploy/noinit-hosts.txt and fill it in
HOSTS_FILE="$ROOT/deploy/noinit-hosts.txt"
STATE_DIR="${HOME}/.local/state/ytmon"
LOG="$STATE_DIR/watchdog.log"
OPTS="-n -o BatchMode=yes -o ConnectTimeout=6 -o StrictHostKeyChecking=accept-new"

mkdir -p "$STATE_DIR"

log() { echo "[$(date '+%F %T')] $*" >> "$LOG"; }

[ -f "$HOSTS_FILE" ] || { log "missing $HOSTS_FILE"; exit 1; }

while read -r ip name ctid; do
    case "$ip" in ''|'#'*) continue ;; esac
    out=$(ssh $OPTS "root@$ip" '
        # Zombies must not count as alive: these containers run the app as PID 1
        # and never call wait(), so every agent we start and later replace
        # leaves a Z entry behind. `pidof` still reports those, which made the
        # watchdog think the agent was running while it was not — and the same
        # blind spot let five dead entries pile up on three hosts.
        alive() {
            n=0
            for d in /proc/[0-9]*; do
                [ -r "$d/comm" ] || continue
                [ "$(cat "$d/comm" 2>/dev/null)" = "ytmon-agent" ] || continue
                st=$(awk "{print \$3}" "$d/stat" 2>/dev/null)
                [ "$st" = "Z" ] && continue
                n=$((n+1))
            done
            echo "$n"
        }
        if [ "$(alive)" -gt 0 ]; then
            echo ALIVE; exit 0
        fi
        mkdir -p /var/lib/ytmon
        setsid /usr/local/bin/ytmon-agent --config /etc/ytmon/agent.toml \
            </dev/null >>/var/log/ytmon-agent.log 2>&1 &
        sleep 2
        if [ "$(alive)" -gt 0 ]; then
            echo STARTED
        else
            echo FAILED
        fi
    ' 2>/dev/null)

    case "$out" in
        ALIVE)   ;;
        STARTED) log "started $name ($ip ct$ctid)" ;;
        *)       log "FAIL $name ($ip ct$ctid): ${out:-ssh-no-response}" ;;
    esac
done < "$HOSTS_FILE"

# 日志超过 2000 行就截断，避免长期占空间
if [ -f "$LOG" ] && [ "$(wc -l < "$LOG")" -gt 2000 ]; then
    tail -n 500 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

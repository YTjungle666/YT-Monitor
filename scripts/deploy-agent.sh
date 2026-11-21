#!/bin/bash
# 幂等下发 ytmon-agent 到一台或多台主机。
#
#   scripts/deploy-agent.sh 192.0.2.40 web-01
#   scripts/deploy-agent.sh --local
#
# 远端：传二进制 -> 写配置 -> 交由 install-agent-init.sh 自动注册服务。
# 每次替换二进制后重启服务，未变化时也算成功（幂等优先于省事）。
# ctid 由 agent 从根挂载源自动识别，不再作为参数或配置项。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/dist/bin/ytmon-agent"
SERVER="${SERVER:?set SERVER to the ingest URL, e.g. http://monitor.example.com:9180/ingest}"
INTERVAL="${INTERVAL:-60}"
# TOKEN is the ingest bearer token; pass it in the environment so it never has
# to be typed into a config file by hand on seventeen hosts.
TOKEN="${TOKEN:-}"
SSH_OPTS="-o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new"

[ -x "$BIN" ] || { echo "missing $BIN — run: make package"; exit 1; }

write_conf() {
    local host="$1"
    printf 'host = "%s"\nserver = "%s"\ninterval_secs = %s\nmounts = ["/"]\nstate_dir = "/var/lib/ytmon"\n' \
        "$host" "$SERVER" "$INTERVAL"
    [ -n "$TOKEN" ] && printf 'server_token = "%s"\n' "$TOKEN"
    return 0
}

deploy_remote() {
    local target="$1" host="$2"
    echo "--- $target  $host ---"
    cat "$BIN" | ssh $SSH_OPTS "root@$target" \
        'cat > /usr/local/bin/.ytmon-agent.tmp && chmod 755 /usr/local/bin/.ytmon-agent.tmp && mv /usr/local/bin/.ytmon-agent.tmp /usr/local/bin/ytmon-agent' \
        || { echo "FAIL push binary"; return 1; }
    write_conf "$host" | ssh $SSH_OPTS "root@$target" \
        'mkdir -p /etc/ytmon /var/lib/ytmon && umask 022 && cat > /etc/ytmon/agent.toml' \
        || { echo "FAIL push config"; return 1; }
    ssh $SSH_OPTS "root@$target" 'sh -s' < "$ROOT/scripts/install-agent-init.sh"
}

deploy_local() {
    local host="$1"
    echo "--- localhost  $host ---"
    sudo install -m 755 "$BIN" /usr/local/bin/ytmon-agent || return 1
    sudo mkdir -p /etc/ytmon /var/lib/ytmon
    write_conf "$host" | sudo tee /etc/ytmon/agent.toml >/dev/null || return 1
    sudo sh -s < "$ROOT/scripts/install-agent-init.sh"
}

if [ "${1:-}" = "--local" ]; then
    host=$(cat /etc/hostname)
    deploy_local "$host"
    exit $?
fi

[ $# -eq 2 ] || { echo "usage: $0 <ip> <hostname>   |   $0 --local"; exit 1; }
deploy_remote "$@"

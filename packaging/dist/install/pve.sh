#!/bin/sh
# ytmon-agent installer for the PVE host (systemd).
#
# Idempotent: running it again is how you upgrade. It compares the SHA-256 of
# the published binary with the local one and only swaps the file when they
# differ, so a rerun is cheap and safe.
#
#   BASE=http://monitor.example.com:8088 \
#     SERVER_URL=http://monitor.example.com:9180/ingest \
#     sh pve.sh
#
# Read-only in spirit: it installs one binary, one config file and one service,
# and nothing else on the host is touched.
set -eu

# This script cannot know where it was served from, so both endpoints are
# required rather than defaulted to somebody else's host.
BASE="${BASE:?set BASE to the ytmon-dist URL, e.g. http://monitor.example.com:8088}"
HOST_NAME="${HOST_NAME:-$(cat /etc/hostname)}"
SERVER_URL="${SERVER_URL:?set SERVER_URL to the ingest URL, e.g. http://monitor.example.com:9180/ingest}"
# Ingest bearer token. Re-running with TOKEN=... rotates it on this host only.
TOKEN="${TOKEN:-}"

BIN=/opt/ytmon/ytmon-agent
CONF=/etc/ytmon/agent.toml
UNIT=/etc/systemd/system/ytmon-agent.service

if [ "$(id -u)" != 0 ]; then
    echo "需要 root 执行" >&2
    exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
    echo "需要 curl" >&2
    exit 1
fi
if ! command -v systemctl >/dev/null 2>&1; then
    echo "没有 systemd，这台机器不适合用这个脚本" >&2
    exit 1
fi

mkdir -p /opt/ytmon /etc/ytmon /var/lib/ytmon

echo "== 下载 agent =="
curl -fsSL "$BASE/bin/ytmon-agent" -o "$BIN.new"
chmod 755 "$BIN.new"
NEW_SHA=$(sha256sum "$BIN.new" | cut -d' ' -f1)
if [ -f "$BIN" ]; then
    OLD_SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
else
    OLD_SHA=""
fi

if [ "$NEW_SHA" = "$OLD_SHA" ]; then
    echo "已是最新: $NEW_SHA"
    rm -f "$BIN.new"
else
    mv "$BIN.new" "$BIN"
    echo "已安装: $NEW_SHA"
fi

if [ ! -f "$CONF" ]; then
    cat > "$CONF" <<EOF
host = "$HOST_NAME"
server = "$SERVER_URL"
interval_secs = 60
mounts = ["/"]
state_dir = "/var/lib/ytmon"
EOF
    echo "已生成 $CONF (host=$HOST_NAME)"
else
    echo "保留现有 $CONF"
fi

# Token lives outside the "create once" block: rotating it must not require
# deleting the config, and an empty TOKEN leaves whatever is already there.
if [ -n "$TOKEN" ]; then
    sed -i '/^server_token/d' "$CONF"
    printf 'server_token = "%s"\n' "$TOKEN" >> "$CONF"
    echo "已写入 ingest token"
fi

cat > "$UNIT" <<'EOF'
[Unit]
Description=ytmon agent (host metrics)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/ytmon/ytmon-agent --config /etc/ytmon/agent.toml
Restart=always
RestartSec=10
Nice=10

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable ytmon-agent
systemctl restart ytmon-agent
sleep 2

if systemctl is-active --quiet ytmon-agent; then
    echo "== 运行中 =="
else
    echo "启动失败，看日志：" >&2
    journalctl -u ytmon-agent -n 20 --no-pager >&2
    exit 1
fi

echo
echo "上报的接口（应看到 wan/lan，没有 vmbr0/veth）："
/opt/ytmon/ytmon-agent --config "$CONF" --once 2>/dev/null \
    | grep -o '"iface":"[^"]*"' | cut -d'"' -f4 | sort -u | sed 's/^/  /'
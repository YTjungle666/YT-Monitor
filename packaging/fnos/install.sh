#!/bin/bash
# ytmon-agent 安装脚本 —— 飞牛 fnOS / Debian 系（systemd）
#
#   sudo bash install.sh
#
# 幂等：重复执行会覆盖二进制并重启服务，保留已有的 agent.toml。
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "请用 root 运行： sudo bash install.sh"
    exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
DEST=/usr/local/bin/ytmon-agent
CONF_DIR=/etc/ytmon
CONF=$CONF_DIR/agent.toml
STATE_DIR=/var/lib/ytmon
SERVER_URL="${SERVER_URL:?set SERVER_URL, e.g. http://monitor.example.com:9180/ingest}"

echo "== 安装 ytmon-agent =="
install -m 755 "$HERE/ytmon-agent" "$DEST"
mkdir -p "$CONF_DIR" "$STATE_DIR"

if [ ! -f "$CONF" ]; then
    # HOST_NAME must match the name PVE reports, otherwise the agent shows up
    # as a second host next to the probe/PVE-discovered entry (fnOS reports
    # the NAS' own hostname while the hypervisor knows it by another name).
    HOST_NAME="${HOST_NAME:-$(hostname)}"
    printf 'host = "%s"\nserver = "%s"\ninterval_secs = 60\nmounts = ["/"]\nstate_dir = "%s"\n' \
        "$HOST_NAME" "$SERVER_URL" "$STATE_DIR" > "$CONF"
    echo "已生成 $CONF（host=$HOST_NAME）"
else
    echo "保留现有 $CONF"
fi

if [ ! -d /run/systemd/system ]; then
    echo "警告：当前不是 systemd 环境，服务未注册。二进制已装到 $DEST。"
    exit 0
fi

cat > /etc/systemd/system/ytmon-agent.service <<'UNIT'
[Unit]
Description=ytmon host metrics agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/ytmon-agent --config /etc/ytmon/agent.toml
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable ytmon-agent >/dev/null 2>&1
systemctl restart ytmon-agent
sleep 2

echo "== 状态 =="
systemctl is-enabled ytmon-agent || true
systemctl is-active ytmon-agent || true

echo
echo "== 自检（应看到 mem.anon_bytes / net.rx_bytes 等）=="
"$DEST" --config "$CONF" --once 2>/dev/null | head -20 || echo "自检失败，查看 journalctl -u ytmon-agent -n 30"

echo
echo "完成。日志： journalctl -u ytmon-agent -f"

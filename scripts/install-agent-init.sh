#!/bin/sh
# 在目标机上以 root 运行：识别真正的 init 并注册 ytmon-agent。
# 前提：/usr/local/bin/ytmon-agent 与 /etc/ytmon/agent.toml 已就位。
# 幂等：重复执行只重启服务。
#
# 判定依据是 /proc/1/comm，不是"某个目录存在"：
# CT401 上 /etc/systemd 存在但 PID1 是 apache2，systemctl 根本不可用。
set -u

BIN=/usr/local/bin/ytmon-agent
CONF=/etc/ytmon/agent.toml
PID1=$(cat /proc/1/comm 2>/dev/null || echo unknown)

if [ ! -x "$BIN" ]; then
    echo "FAIL: $BIN missing or not executable"
    exit 1
fi
if [ ! -f "$CONF" ]; then
    echo "FAIL: $CONF missing"
    exit 1
fi

# --- systemd：必须是 PID1 才可信 ---
if [ "$PID1" = "systemd" ]; then
    printf '%s\n' \
        '[Unit]' \
        'Description=ytmon host metrics agent' \
        'After=network-online.target' \
        'Wants=network-online.target' \
        '' \
        '[Service]' \
        'Type=simple' \
        'ExecStart=/usr/local/bin/ytmon-agent --config /etc/ytmon/agent.toml' \
        'Restart=always' \
        'RestartSec=5' \
        'Environment=RUST_LOG=info' \
        '' \
        '[Install]' \
        'WantedBy=multi-user.target' \
        > /etc/systemd/system/ytmon-agent.service
    systemctl daemon-reload
    systemctl enable ytmon-agent >/dev/null 2>&1
    systemctl restart ytmon-agent
    sleep 2
    echo "INIT=systemd ACTIVE=$(systemctl is-active ytmon-agent 2>/dev/null)"
    exit 0
fi

# --- procd (OpenWrt / Kwrt) ---
if [ "$PID1" = "procd" ]; then
    printf '%s\n' \
        '#!/bin/sh /etc/rc.common' \
        'START=99' \
        'STOP=10' \
        'USE_PROCD=1' \
        'PROG=/usr/local/bin/ytmon-agent' \
        'CONF=/etc/ytmon/agent.toml' \
        'start_service() {' \
        '  procd_open_instance' \
        '  procd_set_param command "$PROG" --config "$CONF"' \
        '  procd_set_param respawn 3600 5 5' \
        '  procd_set_param stdout 1' \
        '  procd_set_param stderr 1' \
        '  procd_close_instance' \
        '}' \
        > /etc/init.d/ytmon-agent
    chmod 755 /etc/init.d/ytmon-agent
    /etc/init.d/ytmon-agent enable >/dev/null 2>&1
    /etc/init.d/ytmon-agent restart >/dev/null 2>&1
    sleep 2
    echo "INIT=procd ACTIVE=$(/etc/init.d/ytmon-agent status 2>&1 | tail -1)"
    exit 0
fi

# --- s6-overlay (HomeAssistant 风格)：服务目录 + 通知 supervisor ---
if [ "$PID1" = "s6-svscan" ] && [ -d /etc/services.d ]; then
    mkdir -p /etc/services.d/ytmon-agent
    printf '%s\n' '#!/bin/sh' 'exec /usr/local/bin/ytmon-agent --config /etc/ytmon/agent.toml' \
        > /etc/services.d/ytmon-agent/run
    chmod 755 /etc/services.d/ytmon-agent/run
    s6-svscanctl -a /var/run/s6/services 2>/dev/null || true
    sleep 2
    echo "INIT=s6 ACTIVE=$(s6-svstat /var/run/s6/services/ytmon-agent 2>&1 | head -1)"
    exit 0
fi

# --- OpenRC：PID1 是 busybox init / 容器自带 init，只要 rc-service 可用 ---
if [ -x /sbin/openrc-run ] && [ -x /sbin/rc-service ]; then
    # 容器里的 OpenRC 常常没有被标记为 "booted"，否则 rc-service 会拒绝操作
    mkdir -p /run/openrc
    touch /run/openrc/softlevel
    printf '%s\n' \
        '#!/sbin/openrc-run' \
        'name="ytmon-agent"' \
        'description="ytmon host metrics agent"' \
        '' \
        'command="/usr/local/bin/ytmon-agent"' \
        'command_args="--config /etc/ytmon/agent.toml"' \
        'command_background=true' \
        'pidfile="/run/${RC_SVCNAME}.pid"' \
        'output_log="/var/log/ytmon-agent.log"' \
        'error_log="/var/log/ytmon-agent.log"' \
        '' \
        'depend() { need net; }' \
        > /etc/init.d/ytmon-agent
    chmod 755 /etc/init.d/ytmon-agent
    rc-update add ytmon-agent default >/dev/null 2>&1
    rc-service ytmon-agent restart >/dev/null 2>&1
    sleep 2
    echo "INIT=openrc ACTIVE=$(rc-service ytmon-agent status 2>&1 | tail -1)"
    exit 0
fi

echo "NO_INIT: pid1=$PID1, no usable service manager — host-side watchdog required"
exit 2

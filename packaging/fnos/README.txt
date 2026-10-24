ytmon-agent for 飞牛 fnOS (Debian)
==================================

安装:

    tar xzf ytmon-agent-fnos.tar.gz
    cd ytmon-agent-fnos
    sudo bash install.sh

脚本会:
  1. 安装二进制到 /usr/local/bin/ytmon-agent
  2. 生成 /etc/ytmon/agent.toml（host 取当前 hostname，已存在则保留）
  3. 写入 systemd unit 并 enable + start

验证:
  systemctl status ytmon-agent
  /usr/local/bin/ytmon-agent --config /etc/ytmon/agent.toml --once | head
  监控端查看: http://<monitor>:9180/api/latest?host=你的主机名

日志:
  journalctl -u ytmon-agent -f

卸载:
  sudo systemctl disable --now ytmon-agent
  sudo rm -f /etc/systemd/system/ytmon-agent.service /usr/local/bin/ytmon-agent
  sudo systemctl daemon-reload
  （/etc/ytmon/agent.toml 与 /var/lib/ytmon 可按需保留）

要求:
  - x86_64，systemd
  - 只需出站 TCP 到监控端 9180

换服务器地址:
  SERVER_URL=http://<monitor>:9180/ingest sudo -E bash install.sh
  或直接编辑 /etc/ytmon/agent.toml 后 systemctl restart ytmon-agent

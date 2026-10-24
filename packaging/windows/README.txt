ytmon-agent for Windows
=======================

安装（以管理员身份打开 PowerShell）:

    powershell -ExecutionPolicy Bypass -File install.ps1

脚本会做四件事:
  1. 复制 ytmon-agent.exe 到 C:\Program Files\ytmon
  2. 生成 agent.toml（host 取计算机名，如已存在则保留）
  3. 添加出站防火墙规则 ytmon-agent
  4. 注册为自动启动的服务 ytmon-agent 并立刻启动

验证:
  Get-Service ytmon-agent        应显示 Running / Automatic
  在监控端查看: http://<monitor>:9180/api/latest?host=你的计算机名

日志:
  C:\ProgramData\ytmon\agent.log
  （服务模式没有控制台，日志只写文件）

卸载:
  powershell -ExecutionPolicy Bypass -File uninstall.ps1

要求:
  - Windows 10 / 11 / Server 2016 及以上（x64）
  - 只需出站 TCP 到监控端 9180，不需要任何入站端口

说明:
  采集的指标是 mem.used_bytes / cpu.usage_pct / fs.* / net.rx_bytes /
  proc.count / uptime.seconds。Windows 没有 cgroup，CPU 是使用率而非
  累计计数器，所以指标名与 Linux 侧略有差异，看板会分别显示。

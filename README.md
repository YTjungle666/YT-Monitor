# YT-Monitor — 家庭网络监控（纯 Rust）

[![ci](https://github.com/YTjungle666/YT-Monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/YTjungle666/YT-Monitor/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue)](LICENSE)

自托管的主机 / 容器监控：cgroup 精确的采集、redb 存储、内置看板、Telegram 告警、
12 个月保留与降采样，外加一个只读分发服务做在线更新。整套是几个静态 musl 二进制，
一个中心进程 + 每台机器一个 agent。

## 为什么不用 node_exporter / Vector

容器里的 `/proc/meminfo` 报的是**宿主机的内存**（除非 hypervisor 做了 lxcfs 虚拟化），
`/proc/diskstats` 报的也是宿主设备 —— 任何读 `/proc` 的采集器在 LXC 里都会集体误报。
唯一正确来源是 cgroup v2：

| 指标 | 正确来源 |
|---|---|
| 内存 | `/sys/fs/cgroup/memory.stat`（`anon` / `file` 分项） |
| CPU | `/sys/fs/cgroup/cpu.stat`（`usage_usec` 累计值） |
| 块设备 IO | `/sys/fs/cgroup/io.stat` |
| 进程数 | `/sys/fs/cgroup/pids.current` |

这是这个项目唯一的存在理由：**在 cgroup 里读 cgroup，而不是读 `/proc`。**

## 组成

| crate | 角色 |
|---|---|
| `ytmon-core` | 线路协议、series 身份、counter 重置判定 |
| `ytmon-agent` | 每台机器一个：采集 → gzip → POST，断网落盘补发 |
| `ytmon-server` | 中心：ingest、redb 存储、查询 API、看板、告警、保留策略、PVE 发现、主动探测 |
| `ytmon-dist` | 只读分发服务：二进制、安装脚本、`version.json` 清单（sha256 + 大小） |

模块边界固定，每个文件只做一件事：`main.rs` 只装配，其余按职责分文件
（`api.rs` / `web.rs` / `store.rs` / `probe.rs` / `pve.rs` / `retention.rs` / `alert/`）。

## 快速开始

```bash
# 1. 构建（需要 x86_64-unknown-linux-musl target + musl-gcc）
make build        # release，静态 musl
make package      # dist/bin/* 与 SHA256SUMS

# 2. 起中心（空配置即可跑：只 ingest + 出图）
./target/x86_64-unknown-linux-musl/release/ytmon-server \
    --listen 0.0.0.0:9180 --db /var/lib/ytmon/ytmon.db

# 3. 每台机器放一个 agent
mkdir -p /etc/ytmon
cat > /etc/ytmon/agent.toml <<'EOF'
server = "http://monitor.example.com:9180/ingest"
interval_secs = 60
EOF
./ytmon-agent --config /etc/ytmon/agent.toml          # 前台跑
./ytmon-agent --config /etc/ytmon/agent.toml --once   # 只采集一次并打印 JSON
```

看板在 `http://<monitor>/`（面板监听用 `extra_listen` 指定），15 秒自动刷新。

## 配置

`/etc/ytmon/server.toml` —— 每个字段都可选，缺文件时只 ingest + 出图：

```toml
listen = "0.0.0.0:9180"
extra_listen = "0.0.0.0:80"                 # 看板走 80，ingest 保留自己的端口
db = "/var/lib/ytmon/ytmon.db"

pve_base = "https://pve.example.com:8006"   # 只读发现没装 agent 的 CT/VM
pve_node = "pve"
pve_token_file = "/etc/ytmon/pve.token"

telegram_token_file = "/etc/ytmon/telegram.token"
telegram_chat_id = "123456789"
alert_rules_file = "/etc/ytmon/rules.toml"

retention_enabled = true     # raw 14d / hourly 365d / daily 730d
stale_host_days = 0          # 0 = 关机的主机不丢历史

[[probes]]
name = "nas"
kind = "icmp"
addr = "192.0.2.30"
```

`/etc/ytmon/agent.toml`：

```toml
host = "web-01"              # 覆盖上报主机名，默认 /etc/hostname
ctid = 400                   # 仅展示与对账
server = "http://monitor.example.com:9180/ingest"
interval_secs = 60
mounts = ["/"]
state_dir = "/var/lib/ytmon" # 断网时的本地 spool
server_token = "..."         # 可选：服务端要求 Bearer 时填
```

`/etc/ytmon/rules.toml` —— 每轮重读，改完不用重启；完整示例见 `deploy/rules.toml`：

```toml
[[rules]]
name = "cpu_high"
title = "CPU 占用过高"
kind = "threshold"           # threshold | stale
metric = "cpu.usage_pct"
hosts = ["*"]                # "*" / 精确名 / "web-*" 前缀通配
op = "gt"
warning = 70
critical = 90
for_secs = 300
```

## 指标

```
mem.anon_bytes   mem.file_bytes   mem.current_bytes   mem.dirty_bytes
mem.limit_bytes  mem.oom_kill_total
cpu.usage_usec   cpu.throttled_total   proc.count
io.read_bytes    io.write_bytes
net.rx_bytes     net.tx_bytes          (label iface)
fs.used_bytes    fs.total_bytes        (label mount)
uptime.seconds   load.1m
```

计数器一律上报原始单调值，差分与重置检测在服务端完成 —— agent 重启不会伪造一个尖峰。

## API

| 端点 | 说明 |
|---|---|
| `POST /ingest` | agent 上报，支持 `Content-Encoding: gzip`；配了 `server_token` 就要 Bearer |
| `GET /healthz` | 存活性 + series 数，给 dead-man 探活用 |
| `GET /api/hosts` | 已上报主机列表 |
| `GET /api/latest?host=` | 该主机所有 series 的最新值 |
| `GET /api/query?series=&from=&to=` | 单条 series 的时间点，自动选 raw / 1h / 1d |
| `GET /api/overview` | 看板用的一次性快照 |
| `GET /` | 内置看板 |

`:9180`（机器面）只认 ingest 与查询；看板与图标挂在面板口，两个监听不共用路由。

## 在线更新（ytmon-dist）

只读 HTTP 服务，把 `root/bin` 与 `root/install` 下的文件连同 sha256 清单一起发布，
于是"要不要更新"在下载前就能判断：

```
/version.json        # 每个文件的大小、sha256、mtime
/bin/ytmon-agent     # 二进制
/install/pve.sh      # 幂等安装：比对 sha256，相同就跳过
```

脚本不知道自己是哪个地址被拉下来的，所以两个地址显式给：

```sh
curl -fsSL http://monitor.example.com:8088/install/pve.sh -o pve.sh
BASE=http://monitor.example.com:8088 \
SERVER_URL=http://monitor.example.com:9180/ingest sh pve.sh
```

分发包（Windows zip / fnOS tar.gz）由 `scripts/package-targets.sh` 生成。

## 平台

平台差异只落在 `agent/collect/`：

| | Linux | Windows |
|---|---|---|
| 内存 | cgroup v2 `memory.stat`（anon/file 分项） | `mem.used_bytes` / `mem.total_bytes` |
| CPU | `cpu.usage_usec` 累计值，服务端差分 | `cpu.usage_pct`（Windows 无累计计数器） |
| 磁盘 / 网络 | `statvfs` / `/proc/net/dev` | `fs.*` / `net.*` 同名 |
| 服务托管 | systemd / OpenRC / procd / 容器内 supervisor（无 init 时） | Windows SCM（`windows-service`） |

Windows 交叉构建：

```bash
sudo apt install -y gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release -p ytmon-agent --target x86_64-pc-windows-gnu
```

### 容器里没有 init 时怎么自启

有些 OCI 镜像的 PID1 就是应用本身（AdGuardHome、node、`start.sh`），容器里既没有 rc
也没有 systemd，没人能拉起第二个进程。不去改 vendor 的 entrypoint（升级会被覆盖），
而是把 LXC 的 `entrypoint` 换成自己的包装脚本，一套组合：

| 文件 | 作用 |
|---|---|
| `deploy/yt-init.example` | PID1 包装：拉起 crond 与 agent supervisor，再把应用当子进程跑，转发 TERM/INT |
| `scripts/ytmon-agent-supervisor` | 前台 wait agent，退出 15 秒后重试；pidfile 防重复；日志超 256 KB 截到 500 行 |

```bash
# 容器内
install -m 755 yt-init.example /usr/local/bin/yt-init      # 先把默认命令改成本机原来的
install -m 755 ytmon-agent-supervisor /usr/local/bin/
# PVE
pct set <vmid> --entrypoint /usr/local/bin/yt-init
```

原生 init 能用的容器不必套这一层：OpenRC 用 `supervisor="supervise-daemon"` + `respawn_delay`，
s6 用 `/etc/services.d/<name>/run`（s6 自己会重启）。

包升级同理下放到容器，不需要一台常驻主机定时 SSH 进来：

| 系统 | 装什么 | 谁调度 |
|---|---|---|
| Alpine | `scripts/yt-upgrade-alpine` → `/etc/periodic/daily/yt-upgrade` | 容器自己的 busybox crond（默认 crontab 已有 `run-parts /etc/periodic/daily`） |
| Debian | `scripts/yt-upgrade-debian.sh` + systemd `.service`/`.timer` | `yt-upgrade.timer` |

两个脚本都只升不重启服务，并在结束时清包管理器缓存；日志各自留在 `/var/log/yt-upgrade.log`，
超过 256 KB 自动截断 —— 常驻进程和日志都不该无限长。

## 安全边界

- 默认只监听内网。ingest 若要暴露，先给每台 agent 发独立 `server_token`：身份即 token，
  吊销一台等于删一条，不用换全队的密钥。
- `ytmon-dist` 只读，没有上传端点。更新靠往目录里放文件，攻击面就是两个目录的读权限。
- Telegram token、PVE token 都是 600 权限的单行文件，不进配置文件，不进仓库。
- 看板本身不带鉴权，设计前提是只在内网或 VPN 后面。

## 开发

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
```

CI 跑 fmt + clippy（`-D warnings`）+ 三平台构建；tag 触发 release 打包。

## 许可

[PolyForm Noncommercial License 1.0.0](LICENSE)。个人使用、学习、改造随意，商业用途需另行授权。

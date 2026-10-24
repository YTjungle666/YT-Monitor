#!/bin/bash
# 生成可分发的安装包：
#   dist/packages/ytmon-agent-windows-x64.zip
#   dist/packages/ytmon-agent-fnos.tar.gz
#
# 依赖：cargo 交叉编译产物（windows）与 dist/bin （linux musl）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/dist/packages"
mkdir -p "$OUT"

WIN_EXE="$ROOT/target/x86_64-pc-windows-gnu/release/ytmon-agent.exe"
LIN_BIN="$ROOT/dist/bin/ytmon-agent"

[ -f "$WIN_EXE" ] || { echo "缺少 $WIN_EXE —— 先 cargo build --release -p ytmon-agent --target x86_64-pc-windows-gnu"; exit 1; }
[ -f "$LIN_BIN" ] || { echo "缺少 $LIN_BIN —— 先 make package"; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# --- Windows ---
WDIR="$STAGE/ytmon-agent-windows-x64"
mkdir -p "$WDIR"
cp "$WIN_EXE" "$WDIR/ytmon-agent.exe"
cp "$ROOT/packaging/windows/install.ps1" "$ROOT/packaging/windows/uninstall.ps1" "$ROOT/packaging/windows/README.txt" "$WDIR/"
python3 - "$WDIR" "$OUT/ytmon-agent-windows-x64.zip" <<'PY'
import os, sys, zipfile
src, dst = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for root, _, files in os.walk(src):
        for f in sorted(files):
            full = os.path.join(root, f)
            arc = os.path.relpath(full, os.path.dirname(src))
            z.write(full, arc)
print("zip:", dst, os.path.getsize(dst), "bytes")
PY

# --- fnOS ---
FDIR="$STAGE/ytmon-agent-fnos"
mkdir -p "$FDIR"
cp "$LIN_BIN" "$FDIR/ytmon-agent"
cp "$ROOT/packaging/fnos/install.sh" "$ROOT/packaging/fnos/README.txt" "$FDIR/"
chmod 755 "$FDIR/ytmon-agent" "$FDIR/install.sh"
tar -czf "$OUT/ytmon-agent-fnos.tar.gz" -C "$STAGE" ytmon-agent-fnos

echo
echo "== 产物 =="
ls -lh "$OUT"
echo
echo "== 校验 =="
cd "$OUT" && sha256sum ytmon-agent-windows-x64.zip ytmon-agent-fnos.tar.gz | tee SHA256SUMS

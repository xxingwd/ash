#!/usr/bin/env bash
# 构建静态 musl 版 ash 并安装到 ~/.cargo/bin。
# 用法：./scripts/release.sh [install]
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> building static musl release..."
cargo build --release -p ash-cli

BIN="target/x86_64-unknown-linux-musl/release/ash"
strip "$BIN" 2>/dev/null || true

echo "==> artifact: $BIN ($(du -h "$BIN" | cut -f1))"
file "$BIN"

if [[ "${1:-}" == "install" ]]; then
    install -m755 "$BIN" "$HOME/.cargo/bin/ash"
    echo "==> installed to $HOME/.cargo/bin/ash"
fi

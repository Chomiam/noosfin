#!/usr/bin/env bash
# Construit l'archive de release : dist/noosfin-x86_64.tar.gz
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --workspace
out=dist/noosfin
rm -rf dist && mkdir -p "$out"/{bin,session,share}
install -m755 target/release/noosfin target/release/noosfin-installer "$out/bin/"
install -m755 session/noosfin-session session/noosfin-mpv "$out/session/"
install -m644 assets/mpv/noosfin-osc.lua assets/noos-logo.svg "$out/share/"
install -m644 LICENSE README.md "$out/"
tar -C dist -czf dist/noosfin-x86_64.tar.gz noosfin
echo "dist/noosfin-x86_64.tar.gz ($(du -h dist/noosfin-x86_64.tar.gz | cut -f1))"

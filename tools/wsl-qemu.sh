#!/bin/bash
# Set up a private, unprivileged QEMU install inside WSL (Ubuntu).
#
# Why: the QEMU builds for Windows compute SVM segment-base canonicalisation
# with a 32-bit `long`, which truncates every guest GDT/IDT/FS/GS base above
# 32 MiB on VMRUN.  Unikernel guests keep all bases low and are unaffected,
# but Linux guests cannot survive it.  A Linux-built QEMU has a 64-bit `long`
# and works, so `cargo xtask run --wsl` uses this install instead.
#
# Everything lands in $HOME/.conc_os/qemu; nothing is installed system-wide.
set -euo pipefail

ROOT="$HOME/.conc_os/qemu"
DEBS="$ROOT/debs"
PREFIX="$ROOT/root"
mkdir -p "$DEBS" "$PREFIX"

if [ -x "$PREFIX/usr/bin/qemu-system-x86_64" ] && [ -f "$PREFIX/usr/share/OVMF/OVMF_CODE_4M.fd" ]; then
    echo "already set up: $PREFIX"
    exit 0
fi

cd "$DEBS"
echo "resolving dependencies..."
pkgs=$(apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts \
        --no-breaks --no-replaces --no-enhances qemu-system-x86 ovmf \
        | grep -E '^[a-zA-Z0-9]' | sort -u)
missing=""
for p in $pkgs; do
    if ! dpkg -s "$p" >/dev/null 2>&1; then
        missing="$missing $p"
    fi
done
echo "packages to fetch:$(echo $missing | wc -w)"
for p in $missing; do
    if ! ls "$p"_*.deb >/dev/null 2>&1; then
        apt-get download "$p" >/dev/null 2>&1 || echo "  (skip $p)"
    fi
done
echo "extracting..."
for d in "$DEBS"/*.deb; do
    dpkg -x "$d" "$PREFIX"
done
echo "qemu: $("$PREFIX/usr/bin/qemu-system-x86_64" --version | head -1 || true)"
ls -la "$PREFIX/usr/share/OVMF/" | head
echo "done: $PREFIX"

#!/usr/bin/env bash
# Build the guest web server (guest-web/) into images/webcounter as a static
# Linux binary, generating the embedded self-signed certificate on first use.
# Runs under WSL or any Linux with the Go toolchain in ~/.conc_os/go (or PATH).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
export PATH="$HOME/.conc_os/go/bin:$PATH"
export GOCACHE="$HOME/.conc_os/gocache" GOPATH="$HOME/.conc_os/gopath" GOTOOLCHAIN=local
export CGO_ENABLED=0 GOOS=linux GOARCH=amd64
cd "$root/guest-web"
if [ ! -f cert.pem ] || [ ! -f key.pem ]; then
  echo "[build-webcounter] generating self-signed certificate for *.conc"
  go run ./gencert
fi
go build -trimpath -ldflags="-s -w" -o "$root/images/webcounter" .
echo "[build-webcounter] $(go version): $(ls -la "$root/images/webcounter" | awk '{print $5}') bytes -> images/webcounter"

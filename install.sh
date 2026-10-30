#!/bin/sh
set -eu
cd "$(dirname "$0")"
command -v cargo >/dev/null || { echo 'Install Rust using https://rustup.rs first (see README.md).' >&2; exit 1; }
cargo build --release --locked
destination="${PREFIX:-$HOME/.local}/bin"
mkdir -p "$destination"
if [ -f "$destination/electrum-lite" ]; then
    cp -p "$destination/electrum-lite" "$destination/electrum-lite.previous"
fi
install -m 755 target/release/electrum-lite "$destination/electrum-lite"
"$destination/electrum-lite" --version
echo "Installed in $destination; add this directory to PATH. No node or service was started."

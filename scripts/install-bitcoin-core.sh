#!/bin/sh
set -eu
sudo apt-get update
sudo apt-get install -y ca-certificates curl python3 gnupg procps
tmp=$(mktemp "${TMPDIR:-/tmp}/electrum-core-installer.XXXXXXXX")
trap 'rm -f "$tmp"' EXIT HUP INT TERM
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/install-bitcoin-core.py -o "$tmp"
python3 "$tmp"

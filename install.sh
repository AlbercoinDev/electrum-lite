#!/bin/sh
# One-line bootstrap. Trusts GitHub HTTPS and this repository's main branch.
set -eu
command -v python3 >/dev/null 2>&1 || { echo 'Se necesita Python 3.11+.' >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo 'Se necesita curl.' >&2; exit 1; }
temporary=$(mktemp)
trap 'rm -f "$temporary"' EXIT HUP INT TERM
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/install.py \
  -o "$temporary"
python3 "$temporary" "$@"

#!/bin/sh
# One-line bootstrap. Trusts GitHub HTTPS and this repository's main branch.
set -eu
command -v python3 >/dev/null 2>&1 || { echo 'Se necesita Python 3.11+.' >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo 'Se necesita curl.' >&2; exit 1; }
root="$HOME/electrum-lite"
[ ! -L "$root" ] && [ ! -L "$root/tmp" ] || { echo 'Se rechazan directorios enlazados.' >&2; exit 1; }
mkdir -p "$root/tmp"
temporary=$(mktemp "$root/tmp/bootstrap.XXXXXXXX.py")
trap 'rm -f "$temporary"' EXIT
trap 'exit 1' HUP INT TERM
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/main/scripts/bootstrap_bundle.py \
  -o "$temporary"
python3 "$temporary" "$@"

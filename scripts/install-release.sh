#!/bin/sh
set -eu

# This installer is pinned to one experimental build, not the historical tag.
version=thin-live-prune-2026-10-31
asset=electrum-lite-linux-x86_64
sha256=1482cc732da73b83a171fdf31e7f9d65a9352965e98c95182e98f6b406609e17
url="https://github.com/AlbercoinDev/electrum-lite/releases/download/$version/$asset"

test "$(uname -s)" = Linux && test "$(uname -m)" = x86_64 || {
    echo 'Solo se admite Linux x86-64.' >&2; exit 1;
}
for cmd in curl sha256sum mktemp install; do
    command -v "$cmd" >/dev/null || { echo "Falta $cmd." >&2; exit 1; }
done
destination="${PREFIX:-$HOME/.local}/bin"
mkdir -p "$destination"
tmp=$(mktemp "$destination/.electrum-lite-download.XXXXXXXX")
trap 'rm -f "$tmp"' EXIT HUP INT TERM
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 "$url" -o "$tmp"
printf '%s  %s\n' "$sha256" "$tmp" | sha256sum -c -
chmod 755 "$tmp"
"$tmp" --version
if [ -e "$destination/electrum-lite" ]; then
    backup=$(mktemp "$destination/electrum-lite.previous.XXXXXXXX")
    cp -p "$destination/electrum-lite" "$backup"
    echo "Copia del ejecutable anterior: $backup"
fi
mv -f "$tmp" "$destination/electrum-lite"
echo "Instalado en $destination/electrum-lite. No se ha iniciado ningún servicio ni modificado ningún nodo."

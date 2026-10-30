#!/bin/sh
set -eu

# This installer is pinned to one experimental build, not the historical tag.
version=thin-experimental-2026-10-30
asset=electrum-lite-linux-x86_64
sha256=5fed1a39f75abb926cfd2a137cd6e4c8cdd880a3ada743ec61cc002ca6b8e150
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

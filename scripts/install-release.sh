#!/bin/sh
set -eu

# This installer is pinned to one experimental build, not the historical tag.
version=thin-index-preimages-2026-10-31
asset=electrum-lite-linux-x86_64
sha256=3b4027edb4e1c60c2b08aa183a84bf3ce434c83f253098fe7148867a93198d22
url="https://github.com/AlbercoinDev/electrum-lite/releases/download/$version/$asset"
core_installer_url=https://raw.githubusercontent.com/AlbercoinDev/electrum-lite/117da10b27a5d349c7d4f357f7f97365ae30dbd3/scripts/install-bitcoin-core.py
core_installer_sha256=25a15d8dd2f785db797017310f5e46e6403bef5c10b52396c2e497f7316e762a

test "$(uname -s)" = Linux && test "$(uname -m)" = x86_64 || {
    echo 'Solo se admite Linux x86-64.' >&2; exit 1;
}
for cmd in curl sha256sum mktemp install python3 gpg pgrep; do
    command -v "$cmd" >/dev/null || { echo "Falta $cmd." >&2; exit 1; }
done
destination="${PREFIX:-$HOME/.local}/bin"
mkdir -p "$destination"
tmp=$(mktemp "$destination/.electrum-lite-download.XXXXXXXX")
core_installer=$(mktemp "$destination/.electrum-lite-core-installer.XXXXXXXX")
trap 'rm -f "$tmp" "$core_installer"' EXIT HUP INT TERM
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 "$url" -o "$tmp"
printf '%s  %s\n' "$sha256" "$tmp" | sha256sum -c -
chmod 755 "$tmp"
"$tmp" --version
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 "$core_installer_url" -o "$core_installer"
printf '%s  %s\n' "$core_installer_sha256" "$core_installer" | sha256sum -c -
python3 "$core_installer"
if [ -e "$destination/electrum-lite" ]; then
    backup=$(mktemp "$destination/electrum-lite.previous.XXXXXXXX")
    cp -p "$destination/electrum-lite" "$backup"
    echo "Copia del ejecutable anterior: $backup"
fi
mv -f "$tmp" "$destination/electrum-lite"
echo "Instalado en $destination/electrum-lite. No se ha iniciado ningún servicio ni modificado datos de nodos."

#!/bin/sh
# Install only the executable; never change Core, services, or existing data.
set -eu
if [ "$#" -gt 1 ]; then
    echo "Uso: sh install.sh [PREFIJO] (por defecto: \$HOME/.local)" >&2
    exit 1
fi
prefix=${1:-"$HOME/.local"}
case "$prefix" in /*) ;; *) echo "El prefijo debe ser una ruta absoluta." >&2; exit 1 ;; esac
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    echo "Este paquete requiere Linux x86-64; no sirve para ARM, Windows o macOS." >&2
    exit 1
fi
base=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$base"
sha256sum --check SHA256SUMS
./electrum-lite --version
destination="$prefix/bin/electrum-lite"
if [ -e "$destination" ] || [ -L "$destination" ]; then
    echo "Ya existe $destination. Conserva el binario y backup previos; no se sobrescribe." >&2
    exit 1
fi
mkdir -p "$prefix/bin"
temporary=$(mktemp "$prefix/bin/.electrum-lite.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
cp ./electrum-lite "$temporary"
chmod 755 "$temporary"
# Hard-link publication refuses an existing destination, including a racing installer.
ln "$temporary" "$destination"
echo "Instalado en: $destination"
echo "No se han modificado Bitcoin Core, servicios ni datos."
echo "Consulta INSTALL-LINUX.md para conectar y usar el servidor."

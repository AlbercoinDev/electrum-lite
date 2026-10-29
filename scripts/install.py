#!/usr/bin/env python3
"""Unprivileged, binary-only installer/updater. Never opens application data."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

REPOSITORY = "AlbercoinDev/electrum-lite"
API = f"https://api.github.com/repos/{REPOSITORY}/releases/tags/v0.0.1-beta.1"
NAME = re.compile(r"electrum-lite-0\.0\.1-beta\.1-linux-x86_64-([0-9a-f]{7,40})\.tar\.gz")
MAX_ARCHIVE = 128 * 1024 * 1024


def download(url, maximum):
    if not url.startswith("https://"):
        raise RuntimeError("Solo se permiten descargas HTTPS")
    request = urllib.request.Request(url, headers={"User-Agent": "electrum-lite-installer"})
    with urllib.request.urlopen(request, timeout=60) as response:
        if not response.url.startswith("https://"):
            raise RuntimeError("Redirección no HTTPS")
        content = response.read(maximum + 1)
    if len(content) > maximum:
        raise RuntimeError("Descarga demasiado grande")
    return content


def select_assets(release):
    assets = {item["name"]: item for item in release["assets"]}
    candidates = [item for name, item in assets.items()
                  if NAME.fullmatch(name) and name + ".sha256" in assets]
    if not candidates:
        raise RuntimeError("No hay un paquete Linux compatible con checksum")
    package = max(candidates, key=lambda item: (item["created_at"], item["id"]))
    return package, assets[package["name"] + ".sha256"]


def replace_link(path, target):
    temporary = path.with_name(path.name + ".new")
    if temporary.exists() or temporary.is_symlink():
        raise RuntimeError(f"Destino temporal ocupado: {temporary}")
    temporary.symlink_to(target)
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def install(prefix, package, checksum, payload):
    name = package["name"]
    if not NAME.fullmatch(name):
        raise RuntimeError("Nombre de paquete no permitido")
    expected = checksum.decode("ascii").split()
    if (len(expected) != 2 or expected[1] != name
            or not re.fullmatch(r"[0-9a-f]{64}", expected[0])
            or hashlib.sha256(payload).hexdigest() != expected[0]):
        raise RuntimeError("Checksum SHA-256 incorrecto")
    base = prefix / "share/electrum-lite"
    versions = base / "versions"
    versions.mkdir(parents=True, exist_ok=True)
    current = base / "current"
    previous = base / "previous"
    launcher = prefix / "bin/electrum-lite"
    launcher.parent.mkdir(parents=True, exist_ok=True)
    for link in (current, previous):
        if link.exists() and not link.is_symlink():
            raise RuntimeError(f"No se sobrescribe una instalación desconocida: {link}")
    if launcher.exists() or launcher.is_symlink():
        if not launcher.is_symlink() or os.readlink(launcher) != str(current / "electrum-lite"):
            raise RuntimeError(f"Ya existe {launcher}; consérvalo y muévelo antes de instalar")
    identifier = name.removesuffix(".tar.gz")
    destination = versions / identifier
    if destination.exists() or destination.is_symlink():
        if current.is_symlink() and current.resolve() == destination.resolve():
            recorded = (destination / "PACKAGE-SHA256").read_text().strip()
            if recorded != expected[0]:
                raise RuntimeError("El paquete publicado cambió; se rechaza su sustitución")
            if not launcher.is_symlink():
                replace_link(launcher, str(current / "electrum-lite"))
            print(f"Ya está instalada esta entrega: {identifier}")
            return
        raise RuntimeError(f"La entrega ya existe; no se sobrescribe: {destination}")
    with tempfile.TemporaryDirectory(prefix=".install-", dir=versions) as temporary:
        staging = Path(temporary)
        archive = staging / "package.tar.gz"
        archive.write_bytes(payload)
        # Extract only explicitly required regular files; never extract links,
        # devices, arbitrary paths or executable installer code from the tar.
        with tarfile.open(archive, "r:gz") as tar:
            for filename, maximum in (("electrum-lite", MAX_ARCHIVE), ("BUILD-INFO.txt", 65536)):
                members = [m for m in tar.getmembers() if m.name == f"{identifier}/{filename}"]
                if len(members) != 1 or not members[0].isfile() or members[0].size > maximum:
                    raise RuntimeError(f"Contenido de paquete inválido: {filename}")
                with tar.extractfile(members[0]) as source:
                    (staging / filename).write_bytes(source.read(maximum + 1))
        binary = staging / "electrum-lite"
        binary.chmod(0o755)
        result = subprocess.run([str(binary), "--version"], check=True,
                                capture_output=True, text=True, timeout=15)
        if result.stdout.strip() != "electrum-lite 0.0.1-beta.1":
            raise RuntimeError("Versión inesperada del ejecutable")
        archive.unlink()
        (staging / "PACKAGE-SHA256").write_text(expected[0] + "\n")
        os.rename(staging, destination)
    if current.is_symlink():
        replace_link(previous, os.readlink(current))
    replace_link(current, str(destination))
    if not launcher.is_symlink():
        replace_link(launcher, str(current / "electrum-lite"))
    print(f"Instalado: {identifier}\nEjecutable: {launcher}")
    print("Datos, Core y servicios sin modificar. Un proceso ya iniciado sigue usando el binario anterior.")
    print("Antes de reiniciar: detén Lite y crea/verifica un backup con el binario anterior.")
    print(f"Versión anterior conservada en: {previous}")
    if str(launcher.parent) not in os.environ.get("PATH", "").split(os.pathsep):
        print(f'Para usar el nombre corto: export PATH="{launcher.parent}:$PATH"')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, default=Path.home() / ".local")
    args = parser.parse_args()
    if sys.version_info < (3, 11) or platform.system() != "Linux" or platform.machine() != "x86_64":
        raise RuntimeError("Se necesita Linux x86-64 y Python 3.11+")
    if os.geteuid() == 0:
        raise RuntimeError("Ejecuta sin sudo y sin root")
    prefix = args.prefix.expanduser().absolute()
    base = prefix / "share/electrum-lite"
    base.mkdir(parents=True, exist_ok=True)
    with (base / "install.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        release = json.loads(download(API, 4 * 1024 * 1024))
        package, checksum = select_assets(release)
        print("Experimental. Confianza: GitHub HTTPS; SHA-256 no es una firma del editor.")
        install(prefix, package, download(checksum["browser_download_url"], 4096),
                download(package["browser_download_url"], MAX_ARCHIVE))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError, tarfile.TarError) as error:
        sys.exit(f"Instalación cancelada: {error}")

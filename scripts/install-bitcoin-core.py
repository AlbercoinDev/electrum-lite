#!/usr/bin/env python3
"""Install verified upstream Bitcoin Core binaries into a dedicated user prefix."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

KEYS = {
    "achow101": ("1b31ab2e336d5ee44dd4d9f5703ad4445798fcee3b1352ab75afa145e737224a", "152812300785C96444D3334D17565732E08E5E41"),
    "fanquake": ("d9d6d89f5d9da0811ed94469f790d5bd0e185b7ee678e7ca7b3121995ded3d26", "E777299FC265DD04793070EB944D35F9AC3DB76A"),
}
BASE = "https://bitcoincore.org"

def fetch(url, limit):
    request = urllib.request.Request(url, headers={"User-Agent": "electrum-lite-core-installer"})
    with urllib.request.urlopen(request, timeout=120) as response:
        if not response.url.startswith("https://"):
            raise RuntimeError("Redirección no HTTPS rechazada")
        data = response.read(limit + 1)
    if len(data) > limit:
        raise RuntimeError("Descarga supera el límite")
    return data

def main():
    if sys.platform != "linux" or os.uname().machine != "x86_64":
        raise RuntimeError("Solo Linux x86-64 está soportado")
    if os.geteuid() == 0:
        raise RuntimeError("Ejecuta como usuario normal, no con sudo; solo apt usa sudo")
    home = Path.home()
    root = home / ".local/opt/electrum-lite-bitcoin-core"
    bindir = home / ".local/bin"
    names = ("bitcoind", "bitcoin-cli", "bitcoin-tx", "bitcoin-util", "bitcoin-wallet")
    # Avoid accidentally replacing a manually installed executable.
    for name in names:
        path = bindir / name
        if path.exists() or path.is_symlink():
            expected = str(root / "current/bin" / name)
            if not path.is_symlink() or os.readlink(path) != expected:
                raise RuntimeError(f"{path} ya existe y no es gestionado; no se sobrescribe")
    existing = shutil.which("bitcoind")
    if existing and Path(existing) != bindir / "bitcoind":
        raise RuntimeError(f"Existe otro bitcoind en PATH ({existing}); no se modifica ni se sustituye. Retíralo del PATH o actualízalo manualmente")
    result = subprocess.run(["pgrep", "-x", "bitcoind"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if result.returncode != 1:
        raise RuntimeError("Hay un bitcoind en ejecución o no se pudo comprobar; detén Core y vuelve a intentar")
    page = fetch(BASE + "/en/download/", 2_000_000).decode("utf-8")
    matches = set(re.findall(r'/bin/bitcoin-core-([0-9]+(?:\.[0-9]+){1,3})/bitcoin-\1-x86_64-linux-gnu\.tar\.gz', page))
    if len(matches) != 1:
        raise RuntimeError("No se pudo identificar una única versión estable en la página oficial")
    version = matches.pop()
    base = f"{BASE}/bin/bitcoin-core-{version}/"
    archive = f"bitcoin-{version}-x86_64-linux-gnu.tar.gz"
    root.mkdir(parents=True, exist_ok=True)
    bindir.mkdir(parents=True, exist_ok=True)
    current = root / "current"
    if current.exists() or current.is_symlink():
        if not current.is_symlink():
            raise RuntimeError("El directorio current no es un enlace gestionado")
        old = Path(os.readlink(current)).name
        if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,3}", old):
            raise RuntimeError("Destino current no reconocido")
        if tuple(map(int, old.split('.'))) > tuple(map(int, version.split('.'))):
            raise RuntimeError("La versión instalada es más reciente que la página oficial; se rechaza downgrade")
    with tempfile.TemporaryDirectory(prefix="core-install-", dir=root) as temp:
        work = Path(temp)
        gnupg = work / "gnupg"
        gnupg.mkdir(mode=0o700)
        env = dict(os.environ, GNUPGHOME=str(gnupg))
        sums = fetch(base + "SHA256SUMS", 2_000_000)
        (work / "SHA256SUMS").write_bytes(sums)
        (work / "SHA256SUMS.asc").write_bytes(fetch(base + "SHA256SUMS.asc", 2_000_000))
        for name, (digest, _) in KEYS.items():
            key = fetch(f"https://raw.githubusercontent.com/bitcoin-core/guix.sigs/main/builder-keys/{name}.gpg", 1_000_000)
            if hashlib.sha256(key).hexdigest() != digest:
                raise RuntimeError(f"Clave {name} cambió; instalación detenida para revisión")
            file = work / (name + ".gpg")
            file.write_bytes(key)
            subprocess.run(["gpg", "--batch", "--import", str(file)], env=env, check=True, stdout=subprocess.DEVNULL)
        verified = subprocess.run(["gpg", "--batch", "--status-fd", "1", "--verify", str(work / "SHA256SUMS.asc"), str(work / "SHA256SUMS")], env=env, capture_output=True, text=True)
        primary = {line.split()[-1] for line in verified.stdout.splitlines() if line.startswith("[GNUPG:] VALIDSIG ")}
        if not {x[1] for x in KEYS.values()}.issubset(primary) or "[GNUPG:] BADSIG " in verified.stdout:
            raise RuntimeError("Faltan las dos firmas válidas fijadas; no se instalará Core")
        expected = [line.split()[0] for line in sums.decode("ascii").splitlines() if len(line.split()) == 2 and line.split()[1].lstrip('*') == archive]
        if len(expected) != 1 or not re.fullmatch(r"[0-9a-f]{64}", expected[0]):
            raise RuntimeError("Checksum oficial ausente o ambiguo")
        print(f"Descargando Bitcoin Core {version} verificado por dos firmantes…", flush=True)
        data = fetch(base + archive, 300_000_000)
        if hashlib.sha256(data).hexdigest() != expected[0]:
            raise RuntimeError("SHA256 incorrecto; no se instalará Core")
        tar = work / archive
        tar.write_bytes(data)
        stage = work / "release"
        (stage / "bin").mkdir(parents=True)
        with tarfile.open(tar, "r:gz") as source:
            for name in names:
                member = source.getmember(f"bitcoin-{version}/bin/{name}")
                if not member.isfile() or member.size > 150_000_000:
                    raise RuntimeError("Contenido inesperado en archivo oficial")
                with source.extractfile(member) as inp, (stage / "bin" / name).open("xb") as out:
                    shutil.copyfileobj(inp, out)
                (stage / "bin" / name).chmod(0o755)
        subprocess.run([str(stage / "bin/bitcoind"), "--version"], check=True, stdout=subprocess.DEVNULL)
        release = root / "releases" / version
        release.parent.mkdir(exist_ok=True)
        if release.exists():
            # Never overwrite an existing release, including a failed previous attempt.
            for name in names:
                if hashlib.sha256((release / "bin" / name).read_bytes()).digest() != hashlib.sha256((stage / "bin" / name).read_bytes()).digest():
                    raise RuntimeError(f"La instalación existente {release} difiere; se conserva sin cambios")
        else:
            os.rename(stage, release)
        for name in names:
            link = bindir / name
            if not link.is_symlink():
                os.symlink(str(current / "bin" / name), link)
        temporary = root / (".current-" + str(os.getpid()))
        os.symlink(str(release), temporary)
        os.replace(temporary, current)
    print(f"Bitcoin Core {version} instalado en {release}. Los datos y versiones anteriores se conservan.")
    print(f"Ejecutable: {bindir / 'bitcoind'}")
    print('Si no está en PATH: export PATH="$HOME/.local/bin:$PATH"')

if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError, ValueError) as exc:
        print(f"Error: {exc}", file=sys.stderr)
        sys.exit(1)

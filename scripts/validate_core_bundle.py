#!/usr/bin/env python3
"""Authenticate official Core and test a Lite package in disposable offline nodes.

Explicitly opt in to pruning: it generates hundreds of MiB, only in fresh
TemporaryDirectories. No public peers or existing node directories are used.
"""
import argparse
import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

from bundle_policy import CORE_KEY_URL
from core_verify import verify_checksums, verify_package
from install import MAX_ARCHIVE, download


def extract_binary(archive, name, destination):
    members = [member for member in archive.getmembers() if member.name == name]
    if len(members) != 1 or not members[0].isfile() or not 0 < members[0].size <= MAX_ARCHIVE:
        raise RuntimeError("Invalid binary archive member")
    with archive.extractfile(members[0]) as source:
        destination.write_bytes(source.read(MAX_ARCHIVE + 1))
    destination.chmod(0o755)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--version", default="32.1", choices=("32.1",))
    parser.add_argument("--prune", action="store_true")
    parser.add_argument("--temporary-parent", type=Path, default=Path("/tmp/opencode"))
    args = parser.parse_args()
    scripts = Path(__file__).resolve().parent
    package = args.package.resolve()
    with tempfile.TemporaryDirectory(dir=args.temporary_parent, prefix="core-bundle-validation-") as directory:
        root = Path(directory)
        base = f"https://bitcoincore.org/bin/bitcoin-core-{args.version}/"
        sums = download(base + "SHA256SUMS", 1024 * 1024)
        verify_checksums(sums, download(base + "SHA256SUMS.asc", 1024 * 1024),
                         download(CORE_KEY_URL, 1024 * 1024), root)
        filename = f"bitcoin-{args.version}-x86_64-linux-gnu.tar.gz"
        payload = download(base + filename, MAX_ARCHIVE)
        verify_package(sums, filename, payload)
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            for name in ("bitcoind", "bitcoin-cli"):
                extract_binary(archive, f"bitcoin-{args.version}/bin/{name}", root / name)
        with tarfile.open(package) as archive:
            names = [member.name for member in archive.getmembers()
                     if member.name.endswith("/electrum-lite")]
            if len(names) != 1:
                raise RuntimeError("Ambiguous Lite package")
            extract_binary(archive, names[0], root / "electrum-lite")
        environment = dict(os.environ, BITCOIND=str(root / "bitcoind"),
                           BITCOINCLI=str(root / "bitcoin-cli"),
                           ELECTRUM_LITE=str(root / "electrum-lite"), TMPDIR=str(root))
        commands = [[sys.executable, str(scripts / "launcher_smoke.py"), "--package", str(package),
                     "--bitcoind", str(root / "bitcoind"), "--bitcoin-cli", str(root / "bitcoin-cli")]]
        commands += [[sys.executable, str(scripts / name)]
                     for name in ("core_smoke.py", "managed_smoke.py")]
        if args.prune:
            commands.append([sys.executable, str(scripts / "prune_smoke.py")])
        for command in commands:
            print(f"RUN {Path(command[1]).name} with authenticated Core {args.version}", flush=True)
            run_environment = dict(environment)
            if command[1].endswith("prune_smoke.py"):
                run_environment["AUTO_PRUNE"] = "1"
            subprocess.run(command, env=run_environment, check=True)
            print(f"PASS {Path(command[1]).name}", flush=True)
    assert not root.exists()
    print("PASS validation binaries and all node data removed", flush=True)


if __name__ == "__main__":
    main()

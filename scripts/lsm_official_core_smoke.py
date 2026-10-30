#!/usr/bin/env python3
"""Authenticate latest official supported Core, run LSM TCP suite, remove tools.

No existing Core is used; the child suite only creates isolated offline regtest.
"""
import argparse
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

from bundle_policy import CORE_KEY_URL, CORE_RELEASE_API, official_core_release
from core_verify import verify_checksums, verify_package
from install import MAX_ARCHIVE, download


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--electrum-lite", type=Path, required=True)
    parser.add_argument("--suite", choices=("tcp", "prune"), default="tcp",
                        help="prune generates hundreds of MiB in a disposable offline regtest")
    args = parser.parse_args()
    version, base = official_core_release(json.loads(download(CORE_RELEASE_API, 4 * 1024 * 1024)))
    if version != "32.1":
        raise RuntimeError(f"Latest stable Core is {version}; update explicit compatibility validation, no fallback")
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="lsm-official-core-") as directory:
        root = Path(directory)
        sums = download(base + "SHA256SUMS", 1024 * 1024)
        verify_checksums(sums, download(base + "SHA256SUMS.asc", 1024 * 1024),
                         download(CORE_KEY_URL, 1024 * 1024), root)
        filename = f"bitcoin-{version}-x86_64-linux-gnu.tar.gz"
        payload = download(base + filename, MAX_ARCHIVE)
        verify_package(sums, filename, payload)
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            for name in ("bitcoind", "bitcoin-cli"):
                members = [member for member in archive.getmembers()
                           if member.name == f"bitcoin-{version}/bin/{name}"]
                if len(members) != 1 or not members[0].isfile() or not 0 < members[0].size <= MAX_ARCHIVE:
                    raise RuntimeError("Invalid Core executable archive member")
                with archive.extractfile(members[0]) as source:
                    path = root / name
                    path.write_bytes(source.read(MAX_ARCHIVE + 1))
                    path.chmod(0o755)
        print(f"RUN LSM {args.suite} with authenticated official Core {version}", flush=True)
        if args.suite == "prune":
            environment = dict(os.environ, LSM_PRUNE="1", AUTO_PRUNE="1",
                               BITCOIND=str(root / "bitcoind"),
                               ELECTRUM_LITE=str(args.electrum_lite.resolve()))
            subprocess.run([sys.executable, str(Path(__file__).with_name("prune_smoke.py"))],
                           env=environment, check=True, timeout=1200)
        else:
            subprocess.run([sys.executable, str(Path(__file__).with_name("lsm_electrum_smoke.py")),
                        "--bitcoind", str(root / "bitcoind"), "--bitcoin-cli", str(root / "bitcoin-cli"),
                        "--electrum-lite", str(args.electrum_lite.resolve())], check=True, timeout=600)
    assert not root.exists()
    print("PASS official Core tools and signature verification metadata removed")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Offline launcher test with explicit Core and Lite package; always clean data."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

from bundle_policy import bundle_lock


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--bitcoind", type=Path, required=True)
    parser.add_argument("--bitcoin-cli", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="launcher-smoke-") as directory:
        root = Path(directory)
        (root / "scripts").mkdir()
        for name in ("bundle_launcher.py", "bundle_policy.py"):
            shutil.copy2(Path(__file__).parent / name, root / "scripts" / name)
        manifest = {"format": 1, "lite": "versions/lite/" + "a" * 64,
                    "core": "versions/bitcoin-core/" + "b" * 64}
        for kind in ("lite", "core"):
            (root / manifest[kind]).mkdir(parents=True)
        with tarfile.open(args.package) as archive:
            member = next(m for m in archive.getmembers()
                          if m.name.endswith("/electrum-lite") and m.isfile())
            binary = root / manifest["lite"] / "electrum-lite"
            binary.write_bytes(archive.extractfile(member).read())
            binary.chmod(0o755)
        shutil.copy2(args.bitcoind, root / manifest["core"] / "bitcoind")
        (root / "active.json").write_text(json.dumps(manifest))
        with (root / "log").open("w") as log:
            app = subprocess.Popen([sys.executable, str(root / "scripts/bundle_launcher.py"),
                                    "--network", "regtest", "--offline"], stdout=log, stderr=log)
            cookie = root / "data/regtest/core/regtest/.cookie"
            try:
                for _ in range(240):
                    if app.poll() is not None:
                        raise RuntimeError((root / "log").read_text())
                    result = subprocess.run([str(args.bitcoin_cli), "-regtest",
                                             f"-datadir={root}/data/regtest/core", "-rpcport=19443",
                                             "getblockchaininfo"], capture_output=True, text=True)
                    if result.returncode == 0:
                        break
                    time.sleep(.25)
                else:
                    raise RuntimeError("Core startup timeout")
                assert json.loads(result.stdout)["chain"] == "regtest"
                try:
                    with bundle_lock(root, updating=True):
                        raise AssertionError("Updater entered during runtime")
                except RuntimeError:
                    pass
                print("PASS managed offline Core started; updater excluded")
            finally:
                if app.poll() is None:
                    app.terminate()
                try:
                    app.wait(timeout=150)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
                    raise
                print((root / "log").read_text()[-1500:])
            assert app.returncode == 0
            assert not cookie.exists(), "Core not stopped"
            with bundle_lock(root, updating=True):
                pass
            print("PASS SIGTERM closes managed Core; locks released")
    assert not root.exists()
    print("PASS all smoke binaries, node data and temporary files removed")


if __name__ == "__main__":
    main()

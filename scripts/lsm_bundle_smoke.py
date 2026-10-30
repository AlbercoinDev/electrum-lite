#!/usr/bin/env python3
"""Minimal published-package Signet OFFLINE install/start/stop/update check.

No chain download or performance test. Uses a new disposable installation only.
"""
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import time

from install_bundle import install_latest


def main():
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="lsm-bundle-") as directory:
        root = Path(directory) / "electrum-lite"
        manifest = install_latest(root)
        sentinel = root / "data/old-generation-preserved"
        sentinel.write_text("do not migrate")
        log = Path(directory) / "supervisor.log"
        children = []
        with log.open("w") as output:
            app = subprocess.Popen([str(root / "electrum-lite"), "--network", "signet", "--offline"],
                                   stdout=output, stderr=output)
            try:
                deadline = time.monotonic() + 120
                while True:
                    assert app.poll() is None, log.read_text()[-3000:]
                    try:
                        with socket.create_connection(("127.0.0.1", 60601), timeout=2) as stream:
                            stream.settimeout(3)
                            stream.sendall(b'{"id":1,"method":"server.health","params":[]}\n')
                            with stream.makefile("rb") as reader:
                                health = json.loads(reader.readline(65536))["result"]
                        if health.get("backend") == "lsm" and health.get("network") == "signet" and health.get("core_height") is not None:
                            break
                    except (OSError, ValueError, KeyError):
                        pass
                    if time.monotonic() > deadline:
                        raise RuntimeError("Signet offline startup timeout: " + log.read_text()[-3000:])
                    time.sleep(.2)
                children = Path(f"/proc/{app.pid}/task/{app.pid}/children").read_text().split()
                assert len(children) == 2, children
                assert (root / "data/signet/lsm-v3/core/signet/.cookie").is_file()
                assert (root / "data/signet/lsm-v3/lite/signet/lsm").is_dir()
                assert not list((root / "data").rglob("index.sqlite"))
                print("PASS published LSM pair: Signet offline, backend=lsm, owned Core and isolated LSM generation", flush=True)
            finally:
                if app.poll() is None:
                    app.terminate()
                try:
                    app.wait(timeout=150)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
                    raise
            assert app.returncode == 0, log.read_text()[-3000:]
        assert all(not Path(f"/proc/{pid}").exists() for pid in children)
        assert not (root / "data/signet/lsm-v3/core/signet/.cookie").exists()
        assert install_latest(root) == manifest
        assert sentinel.read_text() == "do not migrate"
        print("PASS both children stopped; idempotent update preserved existing data", flush=True)
    assert not root.exists()
    print("PASS disposable installation and all test data removed", flush=True)


if __name__ == "__main__":
    main()

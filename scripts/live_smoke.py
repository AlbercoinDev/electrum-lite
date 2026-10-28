#!/usr/bin/env python3
"""Bounded public Signet/Testnet4 prefix test, WITHOUT wallets or pruning.

Downloads at most approximately the requested network budget (plus in-flight data).
This is NOT sustained synchronization or Sparrow interoperability validation.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--network", choices=["signet", "testnet4"], required=True)
    parser.add_argument("--max-seconds", type=int, default=120)
    parser.add_argument("--max-download-mib", type=int, default=128)
    args = parser.parse_args()
    assert 1 <= args.max_seconds <= 600 and 1 <= args.max_download_mib <= 512
    binary = str(Path(os.environ.get("ELECTRUM_LITE", "target/release/electrum-lite")).resolve())
    bitcoind = os.environ.get("BITCOIND", "bitcoind")
    with tempfile.TemporaryDirectory(prefix=f"electrum-live-{args.network}-") as temporary:
        root = Path(temporary)
        data = root / "core"
        data.mkdir()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        cookie = data / args.network / ".cookie"
        with (root / "core.log").open("w") as log:
            process = subprocess.Popen([
                bitcoind, f"-{args.network}", f"-datadir={data}", f"-rpcport={port}", "-server=1",
                "-listen=0", "-maxconnections=4", "-blocksonly=1", "-dbcache=64", "-prune=1",
                "-txindex=0", "-rpcbind=127.0.0.1", "-rpcallowip=127.0.0.1",
            ], stdout=log, stderr=log)

            def rpc(method, params=None):
                auth = base64.b64encode(cookie.read_bytes().strip()).decode()
                request = urllib.request.Request(f"http://127.0.0.1:{port}/", data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode(), headers={"Authorization": "Basic " + auth, "Content-Type": "application/json"})
                try:
                    response = urllib.request.urlopen(request, timeout=15)
                except urllib.error.HTTPError as error:
                    response = error
                result = json.load(response)
                if result.get("error"):
                    raise RuntimeError(str(result["error"]))
                return result["result"]

            try:
                deadline = time.monotonic() + args.max_seconds
                info = None
                received = 0
                peers = 0
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError((root / "core.log").read_text())
                    try:
                        info = rpc("getblockchaininfo")
                        received = rpc("getnettotals")["totalbytesrecv"]
                        peers = rpc("getnetworkinfo")["connections"]
                        if info["blocks"] >= 100 or received >= args.max_download_mib * 1024 * 1024:
                            break
                    except (OSError, RuntimeError):
                        pass
                    time.sleep(0.2)
                rpc("setnetworkactive", [False])
                info = rpc("getblockchaininfo")
                height = min(info["blocks"], 100)
                if height < 1:
                    print(json.dumps({"network": args.network, "result": "NOT_VALIDATED", "reason": "no validated blocks within bounded P2P window", "peers": peers, "received_bytes": received, "headers": info["headers"]}))
                    return 2
                app = [binary, "--network", args.network, "--data-dir", str(root / "archive")]
                result = subprocess.run([*app, "sync", "--rpc-url", f"http://127.0.0.1:{port}/", "--cookie-file", str(cookie), "--max-blocks", "101", "--to-height", str(height)], capture_output=True, text=True, timeout=120)
                assert result.returncode == 0, result.stderr
                stats = json.loads(result.stdout)
                assert stats["indexed_tip"]["height"] == height
                assert subprocess.run([*app, "audit"], capture_output=True, text=True, timeout=120).returncode == 0
                print(json.dumps({"network": args.network, "result": "PREFIX_VALIDATED", "height": height, "peers": peers, "received_bytes": received, "transactions": stats["transactions"], "full_sync": False}))
                return 0
            finally:
                try:
                    rpc("stop")
                    process.wait(timeout=30)
                except (OSError, RuntimeError, subprocess.TimeoutExpired):
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    raise SystemExit(main())

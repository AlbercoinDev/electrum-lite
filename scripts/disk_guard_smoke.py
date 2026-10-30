#!/usr/bin/env python3
"""Exercise the free-space guard using only an isolated managed regtest."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bitcoind", required=True)
    parser.add_argument("--bitcoin-cli", required=True)
    parser.add_argument("--electrum-lite", required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="disk-guard-smoke-", dir="/tmp/opencode") as temp:
        root = Path(temp)
        stat = os.statvfs(root)
        available = stat.f_bavail * stat.f_frsize
        reserve = available // (1024 * 1024) + 1024
        if reserve > 1048576 or available < 1024**3:
            raise RuntimeError("test requires between 1 GiB and 1 TiB free")
        rpc, p2p, listen = port(), port(), port()
        common = [str(Path(args.electrum_lite).resolve()), "--network", "regtest", "--data-dir", str(root / "lite"), "--core-dir", str(root / "core"), "--bitcoind", args.bitcoind, "--core-rpc", f"http://127.0.0.1:{rpc}", "--core-p2p-port", str(p2p), "--listen", f"127.0.0.1:{listen}", "--confirm-dedicated"]
        events = root / "lite/regtest/supervisor/events.jsonl"

        def network():
            result = subprocess.run([args.bitcoin_cli, "-regtest", f"-datadir={root / 'core'}", f"-rpcport={rpc}", "getnetworkinfo"], capture_output=True, text=True, timeout=5, check=True)
            return json.loads(result.stdout)["networkactive"]

        def run(reserve_mib, expect_paused):
            child = subprocess.Popen(common + ["--min-free-mib", str(reserve_mib), "managed-serve"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    if child.poll() is not None:
                        raise RuntimeError(f"supervisor exited: {child.returncode}")
                    try:
                        entries = [json.loads(line) for line in events.read_text().splitlines()]
                        if expect_paused and any(e["event"] == "backpressure_pause" and e["details"]["reason"] == "disk" for e in entries) and not network():
                            return
                        if not expect_paused and network():
                            return
                    except (OSError, subprocess.CalledProcessError, KeyError):
                        pass
                    time.sleep(0.1)
                raise RuntimeError("network state timeout")
            finally:
                if child.poll() is None:
                    child.send_signal(signal.SIGTERM)
                    child.wait(timeout=45)
                assert child.returncode == 0, child.returncode

        run(reserve, True)
        print("PASS: insufficient configured reserve pauses dedicated Core networking")
        run(256, False)
        print("PASS: restart with sufficient free space permits networking")


if __name__ == "__main__":
    main()

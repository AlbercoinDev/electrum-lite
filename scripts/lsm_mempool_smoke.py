#!/usr/bin/env python3
"""New LSM mempool only: dedicated offline regtest; no existing data touched."""
import argparse
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bitcoind", type=Path, required=True)
    parser.add_argument("--bitcoin-cli", type=Path, required=True)
    parser.add_argument("--example", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="lsm-mempool-core-") as directory:
        root = Path(directory)
        data = root / "core"
        data.mkdir()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        common = ["-regtest", f"-datadir={data}", f"-rpcport={port}"]

        def rpc(*arguments):
            return subprocess.check_output([str(args.bitcoin_cli.resolve()), *common, *arguments],
                                           text=True, stderr=subprocess.DEVNULL, timeout=30).strip()

        with (root / "core.log").open("w") as log:
            core = subprocess.Popen([str(args.bitcoind.resolve()), *common, "-server=1", "-daemon=0",
                                     "-prune=1", "-txindex=0", "-listen=0", "-networkactive=0", "-dnsseed=0",
                                     "-connect=0", "-fallbackfee=0.0002"], stdout=log, stderr=log)
            try:
                for _ in range(240):
                    try:
                        rpc("getblockchaininfo")
                        break
                    except subprocess.CalledProcessError:
                        if core.poll() is not None:
                            raise RuntimeError("Owned Core stopped before RPC startup")
                        time.sleep(.25)
                else:
                    raise RuntimeError("Core startup timeout")
                rpc("createwallet", "lsm-mempool-only-test")
                address = rpc("getnewaddress", "", "bech32")
                rpc("generatetoaddress", "101", address)
                subprocess.run([str(args.example.resolve()), "--confirm-isolated-regtest",
                                f"http://127.0.0.1:{port}/", str(data / "regtest/.cookie"), str(root / "lsm")],
                               check=True, timeout=360)
            finally:
                if core.poll() is None:
                    core.terminate()
                try:
                    core.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    core.kill()
                    core.wait()
                    raise
            assert core.returncode == 0
    assert not root.exists()
    print("PASS owned Core stopped; wallet, cookies, raw transactions, LSM data and all temporaries removed")


if __name__ == "__main__":
    main()

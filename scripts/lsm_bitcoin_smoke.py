#!/usr/bin/env python3
"""Generate a fresh offline Core regtest with a signed witness spend; clean all data."""
import argparse
import json
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
    parser.add_argument("--temporary-parent", type=Path, default=Path("/tmp/opencode"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(dir=args.temporary_parent, prefix="lsm-core-regtest-") as directory:
        root = Path(directory)
        data = root / "core"
        data.mkdir()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        common = ["-regtest", f"-datadir={data}", f"-rpcport={port}"]

        def rpc(*arguments):
            result = subprocess.run([str(args.bitcoin_cli), *common, *arguments],
                                    capture_output=True, text=True, timeout=30, check=True)
            return result.stdout.strip()

        with (root / "core.log").open("w") as log:
            core = subprocess.Popen([str(args.bitcoind), *common, "-server=1", "-daemon=0", "-prune=1", "-txindex=0",
                                     "-listen=0", "-networkactive=0", "-dnsseed=0", "-connect=0", "-fallbackfee=0.0002"],
                                    stdout=log, stderr=log)
            try:
                for _ in range(240):
                    try:
                        info = json.loads(rpc("getblockchaininfo"))
                        break
                    except subprocess.CalledProcessError:
                        if core.poll() is not None:
                            raise RuntimeError((root / "core.log").read_text())
                        time.sleep(.25)
                else:
                    raise RuntimeError("Core startup timeout")
                assert info["chain"] == "regtest"
                assert json.loads(rpc("getnetworkinfo"))["networkactive"] is False
                rpc("createwallet", "lsm-test-only")
                address = rpc("getnewaddress", "", "bech32")
                rpc("generatetoaddress", "101", address)
                rpc("sendtoaddress", rpc("getnewaddress", "", "bech32"), "1")
                rpc("generatetoaddress", "1", address)
                blocks = root / "blocks"
                blocks.mkdir()
                for height in range(103):
                    block = bytes.fromhex(rpc("getblock", rpc("getblockhash", str(height)), "0"))
                    (blocks / f"{height:08}.raw").write_bytes(block)
                subprocess.run([str(args.example.resolve()), str(blocks)], check=True, timeout=300)
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
    print("PASS owned offline Core stopped; generated blocks, wallet and temporary files removed")


if __name__ == "__main__":
    main()

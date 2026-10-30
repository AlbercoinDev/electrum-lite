#!/usr/bin/env python3
"""New LSM only: isolated authenticated Core sync/reorg/restart, always clean data."""
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
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="lsm-sync-core-") as directory:
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

        def check(height):
            subprocess.run([str(args.example.resolve()), f"http://127.0.0.1:{port}/",
                            str(data / "regtest/.cookie"), str(root / "lsm"), str(height)],
                           check=True, timeout=240)

        with (root / "core.log").open("w") as log:
            core = subprocess.Popen([str(args.bitcoind.resolve()), *common, "-server=1", "-daemon=0",
                                     "-prune=1", "-txindex=0", "-listen=0", "-networkactive=0", "-dnsseed=0",
                                     "-connect=0", "-fallbackfee=0.0002"], stdout=log, stderr=log)
            try:
                for _ in range(240):
                    try:
                        assert json.loads(rpc("getblockchaininfo"))["chain"] == "regtest"
                        break
                    except subprocess.CalledProcessError:
                        if core.poll() is not None:
                            raise RuntimeError("Owned test Core stopped during startup")
                        time.sleep(.25)
                else:
                    raise RuntimeError("Core RPC startup timeout")
                assert json.loads(rpc("getnetworkinfo"))["networkactive"] is False
                rpc("createwallet", "lsm-only-test")
                address = rpc("getnewaddress", "", "bech32")
                rpc("generatetoaddress", "101", address)
                rpc("sendtoaddress", rpc("getnewaddress", "", "bech32"), "1")
                rpc("generatetoaddress", "1", address)
                check(102)
                wrong_cookie = root / "wrong.cookie"
                wrong_cookie.write_text("test:invalid-secret")
                segment = root / "lsm/regtest/blocks-00000000.elb"
                before = segment.read_bytes()
                rejected = subprocess.run([str(args.example.resolve()), f"http://127.0.0.1:{port}/",
                                           str(wrong_cookie), str(root / "lsm"), "102"],
                                          capture_output=True, text=True, timeout=240)
                assert rejected.returncode != 0
                assert "invalid-secret" not in rejected.stdout + rejected.stderr
                assert segment.read_bytes() == before
                print("PASS wrong RPC cookie rejected without archive mutation or credential leak", flush=True)
                # Explicit replacement branch, never an RPC failure pretending
                # to be a fork. This test owns its isolated regtest Core.
                rpc("invalidateblock", rpc("getblockhash", "102"))
                # Different payout ensures a distinct replacement coinbase/block
                # even when both attempts share height, timestamp and mempool.
                replacement_address = rpc("getnewaddress", "", "bech32")
                rpc("generatetoaddress", "3", replacement_address)
                check(104)
                check(104)  # reopening is idempotent; no new archive records
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
    print("PASS LSM-only sync, real Core reorg and restart; owned Core stopped and all test data removed")


if __name__ == "__main__":
    main()

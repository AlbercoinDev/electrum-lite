#!/usr/bin/env python3
"""Isolated Core smoke tests. No public peers, shared datadirs, or pruning RPCs.

BITCOIND=/path/to/bitcoind BITCOINCLI=/path/to/bitcoin-cli \
ELECTRUM_LITE=./target/release/electrum-lite python3 scripts/core_smoke.py
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    bitcoind = os.environ.get("BITCOIND", "bitcoind")
    bitcoincli = os.environ.get("BITCOINCLI", "bitcoin-cli")
    binary = str(Path(os.environ.get("ELECTRUM_LITE", "target/release/electrum-lite")).resolve())
    for chain, flag, subdir in [
        ("mainnet", "-chain=main", ""),
        ("signet", "-signet", "signet"),
        ("testnet4", "-testnet4", "testnet4"),
        ("regtest", "-regtest", "regtest"),
    ]:
        with tempfile.TemporaryDirectory(prefix="electrum-lite-core-") as temporary:
            root = Path(temporary)
            datadir = root / "core"
            datadir.mkdir()
            port = free_port()
            common = [flag, f"-datadir={datadir}", f"-rpcport={port}"]
            with (root / "core.log").open("w") as log:
                process = subprocess.Popen(
                    [bitcoind, *common, "-server=1", "-listen=0", "-networkactive=0",
                     "-dnsseed=0", "-discover=0", "-connect=0", "-prune=1",
                     "-txindex=0", "-fallbackfee=0.0002", "-rpcbind=127.0.0.1", "-rpcallowip=127.0.0.1"],
                    stdout=log, stderr=log,
                )

                def rpc(*args):
                    return subprocess.run([bitcoincli, *common, *args], text=True, capture_output=True, timeout=30)

                try:
                    deadline = time.monotonic() + 45
                    while True:
                        ready = rpc("getblockchaininfo")
                        if ready.returncode == 0:
                            break
                        if process.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError((root / "core.log").read_text())
                        time.sleep(0.1)
                    cookie = datadir / subdir / ".cookie"
                    endpoint = f"http://127.0.0.1:{port}/"
                    app = [binary, "--network", chain, "--data-dir", str(root / "archive")]
                    online = ["--rpc-url", endpoint, "--cookie-file", str(cookie)]

                    def run(*args, success=True):
                        result = subprocess.run([*app, *args], text=True, capture_output=True, timeout=60)
                        assert (result.returncode == 0) == success, result.stderr
                        return json.loads(result.stdout) if success else result.stderr

                    info = run("check-core", *online)
                    assert info["chain"] == ("main" if chain == "mainnet" else chain)
                    if chain == "regtest":
                        assert rpc("createwallet", "smoke").returncode == 0
                        address = rpc("getnewaddress").stdout.strip()
                        assert rpc("generatetoaddress", "105", address).returncode == 0
                        payment = rpc("sendtoaddress", address, "1.0")
                        assert payment.returncode == 0, payment.stderr
                        txid = payment.stdout.strip()
                        assert rpc("generatetoaddress", "1", address).returncode == 0
                        stats = run("sync", *online, "--max-blocks", "200")
                        assert stats["indexed_tip"]["height"] == 106
                        fetched = run("transaction", txid)
                        expected = json.loads(rpc("gettransaction", txid).stdout)["hex"]
                        assert expected[8:12] == "0001", "smoke spend must include SegWit witness"
                        assert fetched[0]["hex"] == expected
                        # Resume performs no duplicate appends.
                        assert run("sync", *online)["transactions"] == stats["transactions"]
                        assert run("inspect")["indexed_tip"] == stats["indexed_tip"]
                        # An active chain mismatch fails closed; no implicit rollback.
                        assert rpc("invalidateblock", stats["indexed_tip"]["hash"]).returncode == 0
                        run("sync", *online, success=False)
                    else:
                        # Offline genesis only: not a live public-network synchronization.
                        stats = run("sync", *online, "--max-blocks", "1")
                        assert stats["indexed_tip"]["height"] == 0
                        assert run("inspect")["transactions"] == 1
                    wrong = subprocess.run(
                        [binary, "--network", "signet" if chain != "signet" else "mainnet", "check-core", *online],
                        text=True, capture_output=True, timeout=30,
                    )
                    assert wrong.returncode != 0
                    print(f"PASS {chain}: isolated Core identity, archive, replay and network rejection")
                finally:
                    try:
                        rpc("stop")
                        process.wait(timeout=20)
                    except (subprocess.TimeoutExpired, OSError):
                        process.kill()
                        process.wait()


if __name__ == "__main__":
    main()

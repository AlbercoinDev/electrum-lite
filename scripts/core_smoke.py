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
                        assert stats["durable_tip"] == stats["indexed_tip"]
                        assert stats["index_backend"] == "sqlite-schema-2"
                        assert (root / "archive" / chain / "index.sqlite").is_file()
                        fetched = run("transaction", txid)
                        expected = json.loads(rpc("gettransaction", txid).stdout)["hex"]
                        assert expected[8:12] == "0001", "smoke spend must include SegWit witness"
                        assert fetched[0]["hex"] == expected
                        # Resume performs no duplicate appends.
                        assert run("sync", *online)["transactions"] == stats["transactions"]
                        assert run("inspect")["indexed_tip"] == stats["indexed_tip"]
                        rebuilt = run("rebuild-index", "--output", str(root / "rebuilt.sqlite"))
                        assert rebuilt["durable_tip"] == stats["durable_tip"]
                        # All CLI processes have exited: copy one complete matching generation.
                        run("backup", "--output", str(root / "backup"))
                        run("verify-backup", "--directory", str(root / "backup"))
                        run("restore", "--backup-dir", str(root / "backup"), "--output-root", str(root / "restored"))
                        restored = subprocess.run(
                            [binary, "--network", chain, "--data-dir", str(root / "restored"), "inspect"],
                            text=True, capture_output=True, timeout=60,
                        )
                        assert restored.returncode == 0, restored.stderr
                        assert json.loads(restored.stdout)["durable_tip"] == stats["durable_tip"]
                        # A retained reorg durably rewinds without deleting archived bytes.
                        assert rpc("invalidateblock", stats["indexed_tip"]["hash"]).returncode == 0
                        assert run("sync", *online)["indexed_tip"]["height"] == 105
                        assert rpc("setmocktime", str(int(time.time()) + 30)).returncode == 0
                        generated = rpc("generatetoaddress", "2", address)
                        assert generated.returncode == 0, generated.stderr
                        assert run("sync", *online)["indexed_tip"]["height"] == 107
                        assert run("audit")["audit"] == "ok"
                        from electrum_smoke import test_server
                        test_server(binary, root, cookie, port, rpc, address, txid)
                        assert rpc("generatetoaddress", "400", address).returncode == 0
                        old = run("sync", *online, "--max-blocks", "1000")
                        original_database = (root / "archive" / chain / "index.sqlite").read_bytes()
                        fork = rpc("getblockhash", "100").stdout.strip()
                        assert rpc("invalidateblock", fork).returncode == 0
                        assert rpc("setmocktime", str(int(time.time()) + 120)).returncode == 0
                        assert rpc("generatetoaddress", "410", address).returncode == 0
                        run("sync", *online, success=False)  # Beyond retained undo; never silently reset.
                        assert run("inspect")["indexed_tip"] == old["indexed_tip"]
                        recovered = run("recover-chain", *online, "--output-root", str(root / "deep-recovered"), "--min-free-mib", "8")
                        assert recovered["height"] == 509
                        assert (root / "archive" / chain / "index.sqlite").read_bytes() == original_database
                        verified = subprocess.run([binary, "--network", chain, "--data-dir", str(root / "deep-recovered"), "audit"], text=True, capture_output=True, timeout=60)
                        assert verified.returncode == 0, verified.stderr
                        print("PASS deep reorg: automatic rewind denied, canonical chain recovered into a separate root, original generation unchanged")
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

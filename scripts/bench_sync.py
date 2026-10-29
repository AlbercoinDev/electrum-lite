#!/usr/bin/env python3
"""Reproducible ingestion benchmark on an isolated regtest chain.

Compares the wall-clock time for:
  * Electrum Lite to fetch and index every block from a running Core over RPC;
  * Bitcoin Core to rebuild its own chainstate from the same blocks.

Both measurements start from an empty index/chainstate and end at the same tip.
Core is the only block source, so Electrum Lite cannot beat Core on the same
machine by "skipping work": this measures how much extra time the archive,
SQLite writes and per-block validation cost on top of Core's RPC serving.

The chain is generated once with a Core wallet and then reused. Blocks are
deliberately dense (many transactions per block) because empty regtest blocks
would only measure RPC latency. Timing is warm-cache and single-machine: it is
evidence for relative throughput, not a mainnet promise.

BITCOIND=/path/to/bitcoind BITCOINCLI=/path/to/bitcoin-cli \
ELECTRUM_LITE=./target/release/electrum-lite \
python3 scripts/bench_sync.py --blocks 80 --txs-per-block 200
"""
import argparse
import base64
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from decimal import Decimal


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Core:
    def __init__(self, bitcoind, cli, datadir, port, extra=()):
        self.bitcoind = bitcoind
        self.cli = cli
        self.datadir = datadir
        self.port = port
        self.extra = list(extra)
        self.process = None
        self._conn = None

    def args(self):
        return [
            "-regtest",
            f"-datadir={self.datadir}",
            f"-rpcport={self.port}",
            "-server=1",
            "-listen=0",
            "-networkactive=0",
            "-dnsseed=0",
            "-discover=0",
            "-connect=0",
            "-txindex=0",
            "-fallbackfee=0.0002",
            "-rpcbind=127.0.0.1",
            "-rpcallowip=127.0.0.1",
            *self.extra,
        ]

    def start(self, log_name="core.log", timeout=90):
        log_path = self.datadir.parent / log_name
        self.log_path = log_path
        self.process = subprocess.Popen(
            [self.bitcoind, *self.args()],
            stdout=log_path.open("w"),
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + timeout
        while True:
            try:
                self.http("getblockchaininfo")
                return
            except (RuntimeError, ConnectionError, OSError):
                if self.process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(log_path.read_text()[-4000:])
            time.sleep(0.05)

    def rpc(self, *args, timeout=120):
        return subprocess.run(
            [
                self.cli,
                "-regtest",
                f"-datadir={self.datadir}",
                f"-rpcport={self.port}",
                *args,
            ],
            text=True,
            capture_output=True,
            timeout=timeout,
        )

    def http(self, method, *params, timeout=300):
        """One keep-alive JSON-RPC connection: no bitcoin-cli process per call."""
        auth = base64.b64encode(self.cookie().read_bytes().strip()).decode()
        body = json.dumps(
            {"jsonrpc": "2.0", "id": "bench", "method": method, "params": list(params)}
        )
        last = None
        for _ in range(3):
            try:
                if self._conn is None:
                    self._conn = http.client.HTTPConnection(
                        "127.0.0.1", self.port, timeout=timeout
                    )
                self._conn.request(
                    "POST",
                    "/",
                    body,
                    {"Authorization": f"Basic {auth}", "Content-Type": "application/json"},
                )
                response = self._conn.getresponse()
                payload = json.loads(response.read())
                if payload.get("error"):
                    raise RuntimeError(payload["error"])
                return payload["result"]
            except (ConnectionError, OSError) as error:
                last = error
                if self._conn is not None:
                    self._conn.close()
                    self._conn = None
                time.sleep(0.1)
        raise RuntimeError(f"RPC {method} failed: {last}")

    def text(self, method, *params):
        result = self.http(method, *params)
        if not isinstance(result, str):
            raise RuntimeError(f"{method} did not return a string")
        return result

    def json(self, method, *params):
        return self.http(method, *params)

    def wait_ready(self, timeout=120):
        result = self.rpc("getblockchaininfo")
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip())
        return json.loads(result.stdout)

    def stop(self):
        if self.process is None:
            return
        try:
            self.http("stop")
        except Exception:
            pass
        try:
            self.process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process = None
        if self._conn is not None:
            self._conn.close()
            self._conn = None

    def cookie(self):
        return self.datadir / "regtest" / ".cookie"


def wait_reindexed(core, timeout):
    """RPC can answer before chainstate verification completes."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            state = core.http("getblockchaininfo")
            if (
                not state.get("initialblockdownload", True)
                and state["blocks"] == state["headers"]
                and state.get("verificationprogress", 0) >= 0.9999
            ):
                return state
        except (RuntimeError, ConnectionError, OSError):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError("timed out waiting for Core reindex")
        time.sleep(0.1)


FEE = Decimal("0.0002")


def generate_chain(core, blocks, txs_per_block):
    """Fan out one matured coinbase into `txs_per_block` independent payments.

    Each transaction spends a distinct UTXO, so blocks stay dense without
    hitting the mempool's 25-ancestor limit that chained wallet spends would.
    The first block spends the 201 mined coinbases; every later block spends
    the previous block's fresh outputs.
    """
    core.json("createwallet", "bench")
    address = core.text("getnewaddress")
    # Coinbase maturity is 100 blocks; mine enough mature coins for the first
    # dense block instead of relying on wallet coin selection.
    core.json("generatetoaddress", 111 + txs_per_block, address)
    utxos = [
        (entry["txid"], entry["vout"], Decimal(str(entry["amount"])))
        for entry in core.json("listunspent")
    ]
    if len(utxos) < txs_per_block:
        raise RuntimeError(f"only {len(utxos)} mature UTXOs for a dense block")
    started = time.monotonic()
    for height in range(blocks):
        next_utxos = []
        for txid, vout, amount in utxos[:txs_per_block]:
            inputs = [{"txid": txid, "vout": vout}]
            outputs = {address: str(amount - FEE)}
            raw = core.text("createrawtransaction", inputs, outputs)
            signed = core.json("signrawtransactionwithwallet", raw)
            if not signed.get("complete"):
                raise RuntimeError("wallet could not sign a benchmark transaction")
            new_txid = core.text("sendrawtransaction", signed["hex"])
            next_utxos.append((new_txid, 0, amount - FEE))
        core.json("generatetoaddress", 1, address)
        utxos = next_utxos
        if height % 10 == 9:
            print(f"  generated {height + 1}/{blocks} dense blocks", flush=True)
    return time.monotonic() - started


def directory_size(path):
    total = 0
    for entry in Path(path).rglob("*"):
        if entry.is_file():
            total += entry.stat().st_size
    return total


def measure_lite(binary, root, core, chain_height, name, batch):
    data_dir = root / name
    data_dir.mkdir()
    started = time.monotonic()
    calls = 0
    while True:
        result = subprocess.run(
            [
                binary,
                "--network",
                "regtest",
                "--data-dir",
                str(data_dir),
                "sync",
                "--rpc-url",
                f"http://127.0.0.1:{core.port}/",
                "--cookie-file",
                str(core.cookie()),
                "--max-blocks",
                str(batch),
            ],
            text=True,
            capture_output=True,
            timeout=3600,
        )
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip() or result.stdout.strip())
        calls += 1
        stats = json.loads(result.stdout)
        tip = stats["indexed_tip"]["height"]
        if tip >= chain_height:
            break
    elapsed = time.monotonic() - started
    if tip != chain_height or stats["indexed_tip"]["hash"] != core.json("getbestblockhash"):
        raise RuntimeError("Electrum Lite benchmark ended at a different Core tip")
    return {
        "seconds": elapsed,
        "sync_calls": calls,
        "tip": tip,
        "transactions": stats["transactions"],
        "utxos": stats["utxos"],
        "history_entries": stats["history_entries"],
        "disk_bytes": directory_size(data_dir),
    }


def measure_core_reindex(core, chain_height, timeout):
    started = time.monotonic()
    core.extra = ["-reindex-chainstate"]
    core.start(log_name="reindex.log", timeout=timeout)
    state = wait_reindexed(core, timeout)
    elapsed = time.monotonic() - started
    if state["blocks"] != chain_height:
        raise RuntimeError("Core reindex ended at a different height")
    core.stop()
    return {
        "seconds": elapsed,
        "tip": state["blocks"],
        "disk_bytes": directory_size(core.datadir / "regtest"),
        "chainstate_bytes": directory_size(core.datadir / "regtest" / "chainstate"),
    }


def summarize(label, sample, blocks, transactions):
    rate = blocks / sample["seconds"] if sample["seconds"] else 0
    print(
        f"{label}: {sample['seconds']:.2f}s  "
        f"blocks/s={rate:.1f}  txs/s={transactions / sample['seconds']:.0f}  "
        f"disk={sample['disk_bytes'] / 1e6:.1f}MB  {sample}"
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--blocks", type=int, default=80, help="dense blocks after maturity")
    parser.add_argument("--txs-per-block", type=int, default=200)
    parser.add_argument("--batch", type=int, default=64, help="sync --max-blocks per call")
    parser.add_argument("--rounds", type=int, default=1, help="lite repeats for variance")
    parser.add_argument("--keep", action="store_true", help="keep the benchmark directory")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()

    if args.blocks <= 0 or args.txs_per_block < 0 or args.rounds <= 0 or args.batch <= 0:
        raise SystemExit("invalid benchmark size")

    bitcoind = os.environ.get("BITCOIND", "bitcoind")
    bitcoincli = os.environ.get("BITCOINCLI", "bitcoin-cli")
    binary = str(
        Path(os.environ.get("ELECTRUM_LITE", "target/release/electrum-lite")).resolve()
    )

    temporary = tempfile.mkdtemp(prefix="electrum-lite-bench-")
    root = Path(temporary)
    core = Core(bitcoind, bitcoincli, root / "core", free_port())
    (root / "core").mkdir()
    report = {"blocks": args.blocks, "txs_per_block": args.txs_per_block, "lite": []}
    try:
        print(f"benchmark directory: {root}", flush=True)
        core.start()
        generated = generate_chain(core, args.blocks, args.txs_per_block)
        info = core.json("getblockchaininfo")
        height = info["blocks"]
        transactions = core.json("getchaintxstats")["txcount"]
        report.update({"height": height, "transactions": transactions})
        print(
            f"chain: height={height} transactions={transactions} "
            f"generation={generated:.1f}s",
            flush=True,
        )

        for round_index in range(args.rounds):
            sample = measure_lite(
                binary, root, core, height, f"lite-{round_index}", args.batch
            )
            report["lite"].append(sample)
            summarize(f"electrum-lite[{round_index}]", sample, height, transactions)

        # Reindex runs last so the generated blocks are fully out of Core's
        # chainstate cache and both tools read the same block files.
        core.stop()
        reindex = measure_core_reindex(core, height, args.timeout)
        report["core_reindex_chainstate"] = reindex
        summarize("core -reindex-chainstate", reindex, height, transactions)

        best_lite = min(sample["seconds"] for sample in report["lite"])
        ratio = reindex["seconds"] / best_lite
        report["core_over_lite"] = ratio
        print(f"Core time / Electrum Lite time = {ratio:.2f}x (higher means lite is faster)")
        print(json.dumps(report, indent=2))
    finally:
        core.stop()
        if args.keep:
            print(f"kept {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())

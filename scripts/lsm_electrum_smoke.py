#!/usr/bin/env python3
"""LSM-only Electrum TCP test. Fresh offline Core, owned children, always cleanup."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import tempfile
import time

from lsm_observe import disk_bytes


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Client:
    def __init__(self, address):
        self.socket = socket.create_connection(address, timeout=5)
        self.buffer = b""
        self.identifier = 0
        self.notes = []

    def close(self):
        self.socket.close()

    def receive(self, timeout=30):
        deadline = time.monotonic() + timeout
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.socket], [], [], max(0, remaining))[0]:
                raise TimeoutError("Electrum frame timeout")
            data = self.socket.recv(65536)
            if not data:
                raise RuntimeError("Electrum connection closed")
            self.buffer += data
            assert len(self.buffer) <= 16_300_000
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def result(self, identifier, error=False):
        while True:
            value = self.receive()
            if isinstance(value, dict) and value.get("method"):
                self.notes.append(value)
                assert len(self.notes) <= 512
                continue
            assert value["id"] == identifier, value
            if error:
                assert "error" in value, value
                return value["error"]
            assert "error" not in value, value
            return value["result"]

    def call(self, method, params=None, error=False):
        self.identifier += 1
        self.socket.sendall(json.dumps({"id": self.identifier, "method": method,
                                        "params": [] if params is None else params}).encode() + b"\n")
        return self.result(self.identifier, error)

    def notification(self, method, predicate):
        deadline = time.monotonic() + 30
        while True:
            for index, value in enumerate(self.notes):
                if value["method"] == method and predicate(value["params"]):
                    return self.notes.pop(index)
            value = self.receive(max(.01, deadline - time.monotonic()))
            assert value.get("method"), value
            self.notes.append(value)
            if time.monotonic() >= deadline:
                raise TimeoutError("Subscription notification timeout")


def status(history):
    if not history:
        return None
    return hashlib.sha256("".join(f"{entry['tx_hash']}:{entry['height']}:" for entry in history).encode()).hexdigest()


def verify_branch(hash_hex, position, branch, root_raw):
    value = bytes.fromhex(hash_hex)[::-1]
    for sibling in branch:
        other = bytes.fromhex(sibling)[::-1]
        joined = value + other if position % 2 == 0 else other + value
        value = hashlib.sha256(hashlib.sha256(joined).digest()).digest()
        position //= 2
    assert position == 0 and value == root_raw


def stop(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=150)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bitcoind", type=Path, required=True)
    parser.add_argument("--bitcoin-cli", type=Path, required=True)
    parser.add_argument("--electrum-lite", type=Path, required=True)
    parser.add_argument("--capacity-outputs", type=int, default=0,
                        help="Only run a bounded 1..1024 distinct-output regtest capacity case")
    parser.add_argument("--capacity-payload-bytes", type=int, default=0,
                        help="Add up to 900000 OP_RETURN bytes to the capacity transaction")
    args = parser.parse_args()
    if not 0 <= args.capacity_outputs <= 1024:
        parser.error("capacity outputs must be 0..1024")
    if not 0 <= args.capacity_payload_bytes <= 900000 or (args.capacity_payload_bytes and not args.capacity_outputs):
        parser.error("payload requires capacity mode and must be 0..900000 bytes")
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="lsm-electrum-core-") as directory:
        root = Path(directory)
        data = root / "core"
        data.mkdir()
        rpc_port, tcp_port = port(), port()
        while tcp_port == rpc_port:
            tcp_port = port()
        common = ["-regtest", f"-datadir={data}", f"-rpcport={rpc_port}"]

        def rpc(method, *arguments):
            # Avoid Linux per-argument size limits for near-weight-limit blocks.
            return subprocess.check_output([str(args.bitcoin_cli.resolve()), *common, "-stdin", method],
                                           input="".join(str(argument) + "\n" for argument in arguments),
                                           text=True, stderr=subprocess.DEVNULL, timeout=30).strip()

        def rpc_json(method, *arguments):
            return json.loads(rpc(method, *arguments))

        def address():
            address = rpc("getnewaddress", "", "bech32")
            script = bytes.fromhex(rpc_json("getaddressinfo", address)["scriptPubKey"])
            return address, hashlib.sha256(script).digest()[::-1].hex()

        def signed(previous, destination, value):
            inputs = [{"txid": previous[0], "vout": previous[1], "sequence": 4294967293}]
            raw = rpc("createrawtransaction", json.dumps(inputs), json.dumps({destination: value}))
            result = rpc_json("signrawtransactionwithwallet", raw)
            assert result["complete"]
            return result["hex"], rpc_json("decoderawtransaction", result["hex"])["txid"]

        def ready(client, height=None):
            deadline = time.monotonic() + 120
            while True:
                health = client.call("server.health")
                if health.get("ready") and (height is None or health.get("indexed_tip", {}).get("height") == height):
                    assert health["backend"] == "lsm"
                    return health
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"LSM readiness timeout: {health}")
                time.sleep(.1)

        def start_server(log, limit=1000, cookie=None):
            return subprocess.Popen([str(args.electrum_lite.resolve()), "--network", "regtest",
                                     "--data-dir", str(root / "lsm"), "--max-query-results", str(limit),
                                     "lsm-serve", "--rpc-url", f"http://127.0.0.1:{rpc_port}/",
                                     "--cookie-file", str(cookie or data / "regtest/.cookie"), "--listen", f"127.0.0.1:{tcp_port}",
                                     "--poll-milliseconds", "250", "--min-free-mib", "64", "--max-subscriptions", "4", "--max-connections", "4"],
                                    stdout=log, stderr=log)

        app = None
        clients = []
        stopped_core = False
        with (root / "core.log").open("w") as core_log, (root / "lsm.log").open("w") as app_log:
            core = subprocess.Popen([str(args.bitcoind.resolve()), *common, "-server=1", "-daemon=0", "-prune=1",
                                     "-txindex=0", "-listen=0", "-networkactive=0", "-dnsseed=0", "-connect=0", "-fallbackfee=0.0002"],
                                    stdout=core_log, stderr=core_log)
            try:
                for _ in range(240):
                    try:
                        assert rpc_json("getblockchaininfo")["chain"] == "regtest"
                        break
                    except subprocess.CalledProcessError:
                        assert core.poll() is None
                        time.sleep(.25)
                else:
                    raise RuntimeError("Owned Core startup timeout")
                assert not rpc_json("getnetworkinfo")["networkactive"]
                rpc("createwallet", "lsm-electrum-only-test")
                mining, mining_script = address()
                rpc("generatetoaddress", 101, mining)
                capacity = None
                if args.capacity_outputs:
                    funding = rpc_json("listunspent", 101)[0]
                    outputs = {}
                    scripts = []
                    for index in range(args.capacity_outputs):
                        destination, script = address()
                        scripts.append(script)
                        # Exact decimal strings avoid binary float accumulation.
                        outputs[destination] = "0.00100000" if index + 1 < args.capacity_outputs else f"{(4_999_900_000 - 100_000 * index) / 100_000_000:.8f}"
                    if args.capacity_payload_bytes:
                        outputs["data"] = "00" * args.capacity_payload_bytes
                    raw = rpc("createrawtransaction", json.dumps([{"txid": funding["txid"], "vout": funding["vout"]}]), json.dumps(outputs))
                    signed_tx = rpc_json("signrawtransactionwithwallet", raw)
                    assert signed_tx["complete"]
                    transaction = rpc_json("decoderawtransaction", signed_tx["hex"])
                    assert any(entry.get("txinwitness") for entry in transaction["vin"])
                    rpc("generateblock", mining, json.dumps([signed_tx["hex"]]))
                    block_weight = rpc_json("getblock", rpc("getblockhash", 102))["weight"]
                    assert block_weight <= 4_000_000
                    capacity = (scripts, signed_tx["hex"], transaction["txid"], block_weight)
                    capacity_started = time.monotonic()
                app = start_server(app_log)
                for _ in range(240):
                    assert app.poll() is None, (root / "lsm.log").read_text()[-2000:]
                    try:
                        client = Client(("127.0.0.1", tcp_port))
                        break
                    except OSError:
                        time.sleep(.1)
                else:
                    raise RuntimeError("LSM TCP startup timeout")
                clients.append(client)
                diagnostic = Client(("127.0.0.1", tcp_port))
                clients.append(diagnostic)
                if capacity:
                    ready(diagnostic, 102)
                    synced_seconds = time.monotonic() - capacity_started
                    scripts, raw, txid, block_weight = capacity
                    assert client.call("blockchain.transaction.get", [txid]) == raw
                    for index, script in enumerate(scripts):
                        coins = client.call("blockchain.scripthash.listunspent", [script])
                        assert len(coins) == 1 and coins[0]["tx_hash"] == txid and coins[0]["tx_pos"] == index
                        assert len(client.call("blockchain.scripthash.get_history", [script])) == 1
                    def rss(pid):
                        fields = Path(f"/proc/{pid}/statm").read_text().split()
                        return int(fields[1]) * os.sysconf("SC_PAGE_SIZE")
                    print(json.dumps({"case": "bounded-local-capacity", "outputs": len(scripts),
                                      "height": 102, "transaction_bytes": len(raw) // 2,
                                      "block_weight": block_weight, "op_return_payload_bytes": args.capacity_payload_bytes,
                                      "startup_and_sync_seconds": round(synced_seconds, 3),
                                      "queries_completed_seconds": round(time.monotonic() - capacity_started, 3),
                                      "lite_rss_sample_bytes": rss(app.pid), "core_rss_sample_bytes": rss(core.pid),
                                      "core_disk": disk_bytes(data), "lite_disk": disk_bytes(root / "lsm"),
                                      "scope": "offline synthetic regtest; RSS sample, not peak; no public capacity claim"}, sort_keys=True), flush=True)
                    print("PASS bounded local LSM capacity: distinct UTXO/history and exact witness transaction", flush=True)
                    # Return still executes finally and TemporaryDirectory cleanup.
                    return
                ready(diagnostic, 101)
                assert client.call("server.version", ["fixture", ["1.4", "1.6"]])[1] == "1.4"
                assert client.call("server.version", ["fixture", "1.6"], error=True)["code"] == 1
                features = client.call("server.features")
                assert features["protocol_max"] == features["protocol_min"] == "1.4"
                assert features["genesis_hash"] == rpc("getblockhash", 0)
                extra = []
                try:
                    for _ in range(2):
                        connection = Client(("127.0.0.1", tcp_port))
                        extra.append(connection)
                        assert connection.call("server.ping") is None
                    excess = Client(("127.0.0.1", tcp_port))
                    try:
                        try:
                            excess.call("server.ping")
                        except (OSError, RuntimeError):
                            pass
                        else:
                            raise AssertionError("connection limit was bypassed")
                    finally:
                        excess.close()
                finally:
                    for connection in extra:
                        connection.close()
                chunks = client.call("blockchain.block.headers", {"start_height": 0, "count": 2})
                expected = "".join(rpc("getblockheader", rpc("getblockhash", height), "false") for height in range(2))
                assert chunks == {"count": 2, "hex": expected, "max": 2016}
                checkpoint = client.call("blockchain.block.header", [1, 2])
                verify_branch(rpc("getblockhash", 1), 1, checkpoint["branch"], bytes.fromhex(checkpoint["root"])[::-1])
                genesis = rpc_json("getblock", rpc("getblockhash", 0), 2)["tx"][0]
                genesis_script = hashlib.sha256(bytes.fromhex(genesis["vout"][0]["scriptPubKey"]["hex"])).digest()[::-1].hex()
                assert client.call("blockchain.scripthash.get_history", [genesis_script]) == []
                assert client.call("blockchain.scripthash.get_balance", [genesis_script]) == {"confirmed": 0, "unconfirmed": 0}
                assert client.call("blockchain.transaction.get", [genesis["txid"]]) == rpc("getblock", rpc("getblockhash", 0), 0)[162:]
                assert client.call("blockchain.scripthash.get_balance", {"scripthash": genesis_script, "extra": True}, error=True)["code"] == -32602
                assert client.call("blockchain.scriptpubkey.get_balance", ["00"], error=True)["code"] == -32601
                client.socket.sendall(json.dumps([{"id": 700, "method": "server.ping"}, {"method": "server.ping"}, {"id": 701, "method": "server.health"}]).encode() + b"\n")
                batch = client.receive()
                assert [entry["id"] for entry in batch] == [700, 701]
                client.socket.sendall(b"{bad json\n")
                assert client.receive()["error"]["code"] == -32700
                assert client.call("blockchain.headers.subscribe")["height"] == 101
                destination, script_a = address()
                child_destination, _ = address()
                grand_destination, script_c = address()
                replacement_destination, script_r = address()
                for script_ in (script_a, script_c, script_r):
                    assert client.call("blockchain.scripthash.subscribe", [script_]) is None
                assert client.call("blockchain.scripthash.subscribe", ["00" * 32], error=True)["code"] == 1
                client.socket.sendall(b'{"id":901,"method":"server.pi')
                time.sleep(.6)  # subscription timer cancels a partial read
                client.socket.sendall(b'ng","params":[]}\n')
                assert client.result(901) is None
                print("PASS LSM TCP: negotiation, baseline header chunks/checkpoints, genesis, batches, named params, fragmented frames and limits", flush=True)
                funding = rpc_json("listunspent", 101)[0]
                previous = funding["txid"], funding["vout"]
                raw_parent, parent = signed(previous, destination, 49.999)
                assert client.call("blockchain.transaction.broadcast", [raw_parent]) == parent
                raw_child, child = signed((parent, 0), child_destination, 49.998)
                client.call("blockchain.transaction.broadcast", [raw_child])
                raw_grand, grand = signed((child, 0), grand_destination, 49.997)
                client.call("blockchain.transaction.broadcast", [raw_grand])
                history_a = client.call("blockchain.scripthash.get_history", [script_a])
                assert [entry["height"] for entry in history_a] == [0, -1]
                assert all(entry["fee"] == 100_000 for entry in history_a)
                assert client.call("blockchain.scripthash.listunspent", [script_a]) == []
                assert client.call("blockchain.scripthash.get_balance", [script_c])["unconfirmed"] == 4_999_700_000
                assert client.call("blockchain.transaction.get", [parent]) == raw_parent
                assert client.call("blockchain.transaction.get", [grand, True])["confirmations"] == 0
                histogram = client.call("mempool.get_fee_histogram")
                assert sum(size for _, size in histogram) == sum(rpc_json("getmempoolentry", txid)["vsize"] for txid in (parent, child, grand))
                client.notification("blockchain.scripthash.subscribe", lambda params: params == [script_a, status(history_a)])
                raw_r, replacement = signed(previous, replacement_destination, 49.99)
                client.call("blockchain.transaction.broadcast", [raw_r])
                assert client.call("blockchain.scripthash.get_mempool", [script_a]) == []
                assert client.call("blockchain.transaction.get", [parent], error=True)["code"] == 1
                client.notification("blockchain.scripthash.subscribe", lambda params: params == [script_c, None])
                payout, _ = address()
                rpc("generatetoaddress", 1, payout)
                ready(diagnostic, 102)
                proof = client.call("blockchain.transaction.get_merkle", [replacement, 102])
                header_hex = rpc("getblockheader", rpc("getblockhash", 102), "false")
                verify_branch(replacement, proof["pos"], proof["merkle"], bytes.fromhex(header_hex)[36:68])
                assert client.call("blockchain.transaction.get", [replacement]) == raw_r
                assert client.call("blockchain.transaction.get", [replacement, True])["confirmations"] == 1
                by_position = client.call("blockchain.transaction.id_from_pos", [102, proof["pos"], True])
                assert by_position == {"tx_hash": replacement, "merkle": proof["merkle"]}
                history_r = client.call("blockchain.scripthash.get_history", [script_r])
                confirmed_status = status(history_r)
                client.notification("blockchain.scripthash.subscribe", lambda params: params == [script_r, confirmed_status])
                client.notification("blockchain.headers.subscribe", lambda params: params[0]["height"] == 102)
                client.call("server.ping")
                client.notes.clear()
                old_hash = rpc("getblockhash", 102)
                rpc("invalidateblock", old_hash)
                new_payout, _ = address()
                rpc("generatetoaddress", 3, new_payout)
                ready(diagnostic, 104)
                assert rpc("getblockhash", 102) != old_hash
                assert status(client.call("blockchain.scripthash.get_history", [script_r])) == confirmed_status
                client.notification("blockchain.scripthash.subscribe", lambda params: params == [script_r, confirmed_status])
                new_proof = client.call("blockchain.transaction.get_merkle", [replacement, 102])
                new_header = rpc("getblockheader", rpc("getblockhash", 102), "false")
                verify_branch(replacement, new_proof["pos"], new_proof["merkle"], bytes.fromhex(new_header)[36:68])
                print("PASS LSM TCP: local witness TX/proofs, mempool chains/RBF, confirmation and reorg notifications including unchanged status", flush=True)
                os.kill(core.pid, signal.SIGSTOP)
                stopped_core = True
                try:
                    time.sleep(.6)
                    started = time.monotonic()
                    assert diagnostic.call("server.ping") is None
                    assert not diagnostic.call("server.health")["ready"]
                    assert time.monotonic() - started < 2
                finally:
                    os.kill(core.pid, signal.SIGCONT)
                    stopped_core = False
                ready(diagnostic, 104)
                for client_ in clients:
                    client_.close()
                clients.clear()
                stop(app)
                assert app.returncode == 0
                app = start_server(app_log, limit=1)
                for _ in range(240):
                    assert app.poll() is None
                    try:
                        client = Client(("127.0.0.1", tcp_port))
                        break
                    except OSError:
                        time.sleep(.1)
                else:
                    raise RuntimeError("LSM restart TCP timeout")
                clients.append(client)
                ready(client, 104)
                assert client.call("blockchain.scripthash.get_history", [mining_script], error=True)["code"] == 1
                assert client.call("blockchain.scripthash.get_balance", [mining_script])["confirmed"] > 0
                assert client.call("blockchain.transaction.get", [replacement]) == raw_r
                print("PASS LSM TCP: nonblocking diagnostics during Core stall, graceful shutdown/reopen and strict response limits without truncation", flush=True)
                client.close()
                clients.clear()
                stop(app)
                assert app.returncode == 0
                app = start_server(app_log, cookie=root / "missing.cookie")
                for _ in range(240):
                    assert app.poll() is None
                    try:
                        client = Client(("127.0.0.1", tcp_port))
                        break
                    except OSError:
                        time.sleep(.1)
                else:
                    raise RuntimeError("LSM unavailable-Core test startup timeout")
                clients.append(client)
                assert client.call("server.features")["protocol_max"] == "1.4"
                assert client.call("blockchain.scripthash.get_balance", [script_r], error=True)["code"] == -32000
                assert not client.call("server.health")["ready"]
                print("PASS LSM TCP: unavailable Core blocks wallet queries even with a complete local index", flush=True)
            finally:
                if stopped_core and core.poll() is None:
                    os.kill(core.pid, signal.SIGCONT)
                for client_ in clients:
                    client_.close()
                try:
                    if app is not None:
                        stop(app)
                finally:
                    stop(core)
            assert core.returncode == 0
    assert not root.exists()
    print("PASS all owned processes stopped; Core wallet/cookies, LSM generations and temporaries removed")


if __name__ == "__main__":
    main()

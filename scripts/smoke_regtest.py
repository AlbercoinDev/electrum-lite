#!/usr/bin/env python3
"""Offline disposable end-to-end smoke. Never uses an existing node or data root."""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import threading
import urllib.request


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait(fn, label):
    deadline = time.monotonic() + 90
    error = None
    while time.monotonic() < deadline:
        try:
            v = fn()
            if v:
                return v
        except (Exception,) as e:
            error = e
        time.sleep(0.25)
    raise RuntimeError(f"timeout: {label}: {error}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--bitcoind", default="bitcoind")
    p.add_argument("--bitcoin-cli", default="bitcoin-cli")
    p.add_argument("--electrum-lite", default="target/release/electrum-lite")
    a = p.parse_args()
    binary = str(Path(a.electrum_lite).resolve())
    temp_base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="electrum-lite-smoke-", dir=temp_base) as temp:
        root = Path(temp)
        rpcport, p2pport, port, proxyport = free_port(), free_port(), free_port(), free_port()
        core_dir = root / "core"
        core_dir.mkdir()
        processes = []
        logs = []
        def launch(args, name):
            f = open(root / (name + ".log"), "w+")
            logs.append(f)
            child = subprocess.Popen(args, stdout=f, stderr=f)
            processes.append(child)
            return child
        def cli(method, *args):
            c = subprocess.run([a.bitcoin_cli, "-regtest", f"-datadir={core_dir}", f"-rpcport={rpcport}", method, *map(str, args)], capture_output=True, text=True, timeout=30)
            if c.returncode:
                raise RuntimeError(c.stderr)
            try:
                return json.loads(c.stdout)
            except json.JSONDecodeError:
                return c.stdout.strip()
        hide_blocks = False
        hold_audit = threading.Event()
        audit_waiting = threading.Event()
        release_audit = threading.Event()
        class Proxy(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                data = self.rfile.read(int(self.headers["Content-Length"]))
                message = json.loads(data)
                if hold_audit.is_set() and message["method"] == "getblock":
                    audit_waiting.set()
                    if not release_audit.wait(timeout=20):
                        raise RuntimeError("audit test gate timed out")
                if hide_blocks and message["method"] == "getblock":
                    result = json.dumps({"id": 1, "result": None, "error": {"code": -1, "message": "simulated pruned block"}}).encode()
                else:
                    request = urllib.request.Request(f"http://127.0.0.1:{rpcport}", data, {"Authorization": self.headers["Authorization"]})
                    with urllib.request.urlopen(request, timeout=30) as r:
                        result = r.read()
                self.send_response(200)
                self.send_header("Content-Length", str(len(result)))
                self.end_headers()
                self.wfile.write(result)
        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", proxyport), Proxy)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        command = [binary, "--network", "regtest", "--data-dir", str(root / "lite"), "--core-cookie", str(core_dir / "regtest" / ".cookie"), "--core-rpc", f"http://127.0.0.1:{proxyport}", "--listen", f"127.0.0.1:{port}", "--block-peer", f"127.0.0.1:{p2pport}"]
        private = root / "private"
        private.mkdir(mode=0o700)
        control = private / "control.sock"
        server_command = command + ["--control-socket", str(control)]
        counter = 0
        def electrum(method, *params):
            nonlocal counter
            counter += 1
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                s.settimeout(15)
                s.sendall(json.dumps({"id": counter, "method": method, "params": params}).encode() + b"\n")
                r = json.loads(s.makefile("rb").readline())
                if "error" in r:
                    raise RuntimeError(r["error"])
                return r["result"]
        try:
            launch([a.bitcoind, "-regtest", f"-datadir={core_dir}", f"-rpcport={rpcport}", f"-port={p2pport}", "-server=1", "-listen=1", f"-bind=127.0.0.1:{p2pport}", "-connect=0", "-dnsseed=0", "-discover=0", "-prune=1", "-fallbackfee=0.00001"], "core")
            wait(lambda: cli("getblockcount") == 0, "Core startup")
            cli("createwallet", "smoke")
            miner = cli("getnewaddress")
            cli("generatetoaddress", 101, miner)
            address = cli("getnewaddress")
            script = bytes.fromhex(cli("getaddressinfo", address)["scriptPubKey"])
            sh = hashlib.sha256(script).digest()[::-1].hex()
            server = launch(server_command + ["serve"], "server")
            wait(lambda: electrum("blockchain.headers.subscribe")["height"] == 101, "initial sync")
            assert electrum("server.version", "smoke", "1.4")[1] == "1.4"
            # The control capability is a private Unix socket, not an Electrum method.
            try:
                electrum("snapshot", str(root / "snapshot-101-1"))
            except RuntimeError:
                pass
            else:
                raise AssertionError("private snapshot exposed over Electrum")
            def snapshot(path):
                with socket.socket(socket.AF_UNIX) as s:
                    s.settimeout(30)
                    s.connect(str(control))
                    s.sendall(json.dumps({"method":"snapshot", "path":str(path)}).encode() + b"\n")
                    return json.loads(s.makefile("rb").readline())
            denied = snapshot(root / "not-a-snapshot")
            assert denied.get("error") and not (root / "not-a-snapshot").exists()
            shot = root / "snapshot-101-1"
            result = snapshot(shot)
            assert result.get("error") is None and result["result"]["height"] == 101 and shot.is_dir()
            assert snapshot(shot).get("error")
            assert electrum("server.ping") is None
            print("PASS: private checkpoint without overwrite or public control method; Electrum responds afterward", flush=True)
            # Audit an independent copy of the checkpoint. Hold the audit at its
            # first block RPC to prove the live Electrum worker can answer while
            # that separate audit process is actively in progress.
            snapshot_root = root / "snapshot-test-root"
            (snapshot_root / "regtest").mkdir(parents=True)
            shutil.copytree(shot, snapshot_root / "regtest/index")
            audit_command = command.copy()
            audit_command[audit_command.index("--data-dir") + 1] = str(snapshot_root)
            hold_audit.set()
            audit_process = launch(audit_command + ["--audit-dir", str(root / "online-audit"), "audit"], "online-audit")
            assert audit_waiting.wait(timeout=20), "audit did not start reading blocks"
            assert audit_process.poll() is None
            assert electrum("server.ping") is None
            release_audit.set()
            hold_audit.clear()
            assert audit_process.wait(timeout=60) == 0
            print("PASS: Electrum answered while a separate snapshot audit was in progress", flush=True)
            with socket.create_connection(("127.0.0.1", port), timeout=5) as batch:
                batch.sendall(json.dumps([{"id": 1, "method": "server.version", "params": ["smoke", ["1.4", "1.4.5"]]}, {"id": 2, "method": "server.ping", "params": []}]).encode() + b"\n")
                replies = json.loads(batch.makefile("rb").readline())
                assert len(replies) == 2 and replies[0]["result"][1] == "1.4" and replies[1]["result"] is None
            txid = cli("sendtoaddress", address, "1.0")
            wait(lambda: any(r["tx_hash"] == txid and r["height"] == 0 for r in electrum("blockchain.scripthash.get_history", sh)), "mempool history")
            assert electrum("blockchain.scripthash.get_balance", sh)["unconfirmed"] == 100_000_000
            replaced = txid
            txid = cli("bumpfee", txid)["txid"]
            wait(lambda: any(r["tx_hash"] == txid for r in electrum("blockchain.scripthash.get_history", sh)), "RBF reconciliation")
            assert all(r["tx_hash"] != replaced for r in electrum("blockchain.scripthash.get_history", sh))
            raw = electrum("blockchain.transaction.get", txid)
            assert cli("decoderawtransaction", raw)["txid"] == txid
            assert electrum("blockchain.transaction.get", txid, True)["txid"] == txid
            assert electrum("mempool.get_fee_histogram")
            with socket.create_connection(("127.0.0.1", port), timeout=5) as notify:
                notify.settimeout(20)
                messages = notify.makefile("rb")
                notify.sendall(json.dumps({"id": 1, "method": "blockchain.headers.subscribe", "params": []}).encode() + b"\n")
                assert "result" in json.loads(messages.readline())
                notify.sendall(json.dumps({"id": 2, "method": "blockchain.scripthash.subscribe", "params": [sh]}).encode() + b"\n")
                assert "result" in json.loads(messages.readline())
                blockhash = cli("generatetoaddress", 1, miner)[0]
                seen = set()
                while len(seen) < 2:
                    message = json.loads(messages.readline())
                    if "method" in message:
                        seen.add(message["method"])
                assert seen == {"blockchain.headers.subscribe", "blockchain.scripthash.subscribe"}
                messages.close()
            wait(lambda: electrum("blockchain.scripthash.get_balance", sh)["confirmed"] == 100_000_000, "confirmed balance")
            unspent = electrum("blockchain.scripthash.listunspent", sh)
            assert any(t["tx_hash"] == txid for t in unspent)
            proof = electrum("blockchain.transaction.get_merkle", txid, 102)
            pos = proof["pos"]
            assert electrum("blockchain.transaction.id_from_pos", 102, pos) == txid
            current = bytes.fromhex(txid)[::-1]
            for sibling in proof["merkle"]:
                sibling = bytes.fromhex(sibling)[::-1]
                data = sibling + current if pos & 1 else current + sibling
                current = hashlib.sha256(hashlib.sha256(data).digest()).digest()
                pos >>= 1
            header = bytes.fromhex(electrum("blockchain.block.header", 102))
            assert current == header[36:68]
            print("PASS: initial sync, JSON batch, mempool/RBF, raw/verbose tx, subscriptions, balance, UTXO, Merkle proof")
            hide_blocks = True
            assert electrum("blockchain.transaction.get", txid) == raw
            cached = list((root / "lite" / "regtest" / "cache").glob("*.blk"))
            assert len(cached) == 1
            # A changed witness keeps the TXID: the witness commitment must reject it.
            original = cached[0].read_bytes()
            witness = bytes.fromhex(cli("decoderawtransaction", raw)["vin"][0]["txinwitness"][0])
            offset = original.index(witness)
            damaged = bytearray(original)
            damaged[offset] ^= 1
            cached[0].write_bytes(damaged)
            try:
                electrum("blockchain.transaction.get", txid)
            except RuntimeError as e:
                assert "verification failed" in str(e)
            else:
                raise AssertionError("corrupt witness accepted")
            cached[0].write_bytes(original)
            assert electrum("blockchain.transaction.get", txid) == raw
            hide_blocks = False
            print("PASS: simulated missing Core block -> witness-block P2P -> persistent cache; corrupt witness rejected")
            cli("invalidateblock", blockhash)
            wait(lambda: electrum("blockchain.headers.subscribe")["height"] == 101, "rewind")
            cli("setmocktime", cli("getblockheader", blockhash)["time"] + 100)
            cli("generatetoaddress", 2, miner)
            wait(lambda: electrum("blockchain.headers.subscribe")["height"] == 103, "new branch")
            server.terminate()
            server.wait(timeout=10)
            server = launch(server_command + ["serve"], "restart")
            wait(lambda: electrum("blockchain.headers.subscribe")["height"] == 103, "restart")
            assert electrum("blockchain.scripthash.get_balance", sh)["confirmed"] == 100_000_000
            server.terminate()
            server.wait(timeout=10)
            subprocess.run(command + ["--backup-dir", str(root / "backup"), "backup"], check=True, timeout=30)
            subprocess.run(command + ["--audit-dir", str(root / "audit"), "audit"], check=True, timeout=60)
            print("PASS: reorg, restart, verified backup, semantic replay audit")
            print("PASS: offline disposable regtest smoke (no existing node touched)")
        except Exception:
            for f in logs:
                f.flush()
                f.seek(0)
                print(f"--- {f.name} ---\n{f.read()[-4000:]}")
            raise
        finally:
            release_audit.set()
            proxy.shutdown()
            for child in reversed(processes):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            for f in logs:
                f.close()


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Real pruning proof on an OWN temporary offline regtest node (hundreds of MiB).

Uses deliberately compressible large OP_RETURN data, NOT a compression benchmark.
BITCOIND=/path/to/bitcoind ELECTRUM_LITE=target/release/electrum-lite python3 scripts/prune_smoke.py
"""
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
from decimal import Decimal
from electrum_smoke import Electrum, proof, wait


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    binary = str(Path(os.environ.get("ELECTRUM_LITE", "target/release/electrum-lite")).resolve())
    bitcoind = os.environ.get("BITCOIND", "bitcoind")
    with tempfile.TemporaryDirectory(prefix="electrum-lite-prune-") as temporary:
        root = Path(temporary)
        data = root / "core"
        data.mkdir()
        rpc_port = port()
        cookie = data / "regtest" / ".cookie"
        with (root / "core.log").open("w") as log:
            node = subprocess.Popen([
                bitcoind, "-regtest", f"-datadir={data}", f"-rpcport={rpc_port}", "-server=1",
                "-listen=0", "-networkactive=0", "-dnsseed=0", "-discover=0", "-connect=0",
                "-prune=1", "-txindex=0", "-fallbackfee=0.0002", "-rpcbind=127.0.0.1", "-rpcallowip=127.0.0.1",
            ], stdout=log, stderr=log)

            def rpc(method, params=None):
                auth = base64.b64encode(cookie.read_bytes().strip()).decode()
                request = urllib.request.Request(f"http://127.0.0.1:{rpc_port}/", data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode(), headers={"Authorization": "Basic " + auth, "Content-Type": "application/json"})
                try:
                    response = urllib.request.urlopen(request, timeout=120)
                except urllib.error.HTTPError as error:
                    response = error
                result = json.load(response)
                if result.get("error"):
                    raise RuntimeError(str(result["error"]))
                return result["result"]

            try:
                deadline = time.monotonic() + 45
                while True:
                    try:
                        rpc("getblockchaininfo")
                        break
                    except (OSError, RuntimeError):
                        if node.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError((root / "core.log").read_text())
                        time.sleep(0.1)
                rpc("createwallet", ["prune-proof"])
                address = rpc("getnewaddress")
                rpc("generatetoaddress", [1000, address])
                target_hash = rpc("getblockhash", [10])
                target_txid = rpc("getblock", [target_hash, 1])["tx"][0]
                target_raw = rpc("getrawtransaction", [target_txid, False, target_hash])
                expected_root = rpc("getblockheader", [target_hash])["merkleroot"]
                script = rpc("getaddressinfo", [address])["scriptPubKey"]
                funding = rpc("listunspent", [100])[0]
                txid, vout, amount = funding["txid"], funding["vout"], Decimal(str(funding["amount"]))
                payload = "00" * (512 * 1024)
                for height in range(600):
                    output_amount = amount - Decimal("0.001")
                    raw = rpc("createrawtransaction", [[{"txid": txid, "vout": vout}], [{address: float(output_amount)}, {"data": payload}], 0, True])
                    signed = rpc("signrawtransactionwithwallet", [raw, [{"txid": txid, "vout": vout, "scriptPubKey": script, "amount": float(amount)}]])
                    assert signed["complete"]
                    rpc("generateblock", [address, [signed["hex"]]])
                    decoded = rpc("decoderawtransaction", [signed["hex"]])
                    txid, vout, amount = decoded["txid"], 0, output_amount
                    if (height + 1) % 100 == 0:
                        print(f"Generated {height + 1}/600 large regtest blocks", flush=True)

                app = [binary, "--network", "regtest", "--data-dir", str(root / "archive")]
                online = ["--rpc-url", f"http://127.0.0.1:{rpc_port}/", "--cookie-file", str(cookie)]

                def run(*args):
                    result = subprocess.run([*app, *args], capture_output=True, text=True, timeout=240)
                    assert result.returncode == 0, result.stderr
                    return json.loads(result.stdout)

                assert run("sync", *online, "--max-blocks", "5000")["indexed_tip"]["height"] == 1600
                result = run("prune", *online, "--confirm", "--backup-dir", str(root / "before-prune"), "--min-free-mib", "8")
                assert result["actual"] >= 10, result
                try:
                    rpc("getblock", [target_hash, 0])
                except RuntimeError as error:
                    assert "pruned" in str(error)
                else:
                    raise AssertionError("historical Core block was not actually pruned")
                assert run("transaction", target_txid)[0]["hex"] == target_raw
                assert (root / "before-prune" / "BACKUP_COMPLETE.json").is_file()
                electrum_port = port()
                with (root / "electrum.log").open("w") as server_log:
                    server = subprocess.Popen([*app, "serve", *online, "--listen", f"127.0.0.1:{electrum_port}", "--poll-seconds", "1", "--min-free-mib", "8"], stdout=server_log, stderr=server_log)
                    client = None
                    try:
                        client = wait(lambda: Electrum(electrum_port), timeout=120)
                        wait(lambda: client.request("server.health")["ready"], timeout=120)
                        health = client.request("server.health")
                        assert health["core_pruned"] is True and health["core_automatic_pruning"] is False
                        assert health["core_prune_height"] >= 10 and health["historical_blocks_missing"] is False
                        assert client.request("blockchain.transaction.get", [target_txid]) == target_raw
                        branch = client.request("blockchain.transaction.get_merkle", [target_txid, 10])
                        assert proof(target_txid, branch["merkle"], branch["pos"]) == expected_root
                    finally:
                        if client:
                            client.close()
                        server.terminate()
                        try:
                            status = server.wait(timeout=30)
                        except subprocess.TimeoutExpired:
                            server.kill()
                            server.wait()
                            raise
                        assert status == 0, (root / "electrum.log").read_text()
                print("PASS REAL PRUNING: Core historical block unavailable; original witness-inclusive TX and valid Merkle proof served from local archive", flush=True)
            finally:
                try:
                    rpc("stop")
                    node.wait(timeout=30)
                except (OSError, RuntimeError, subprocess.TimeoutExpired):
                    node.kill()
                    node.wait()


if __name__ == "__main__":
    main()

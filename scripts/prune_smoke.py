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
    automatic = os.environ.get("AUTO_PRUNE") == "1"
    lsm = os.environ.get("LSM_PRUNE") == "1"
    assert not lsm or automatic, "LSM proof requires dedicated automatic supervisor"
    backend = ["--lsm"] if lsm else []
    with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="electrum-lite-prune-") as temporary:
        root = Path(temporary)
        data = root / "core"
        rpc_port = port()
        cookie = data / "regtest" / ".cookie"
        if automatic:
            # Let the supervisor create its own ownership marker. Never forge
            # ownership or adopt an existing/user datadir in this test.
            bootstrap_port = port()
            with (root / "bootstrap.log").open("w") as bootstrap_log:
                bootstrap = subprocess.Popen([
                    binary, "--network", "regtest", "--data-dir", str(root / "archive"),
                    "managed-serve", *backend, "--bitcoind", bitcoind, "--core-dir", str(data),
                    "--rpc-port", str(rpc_port), "--listen", f"127.0.0.1:{bootstrap_port}",
                    "--confirm-dedicated", "--offline",
                ], stdout=bootstrap_log, stderr=bootstrap_log)
                try:
                    wait(lambda: cookie.exists(), timeout=60)
                finally:
                    bootstrap.terminate()
                    assert bootstrap.wait(timeout=150) == 0, (root / "bootstrap.log").read_text()
        else:
            data.mkdir()
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
                address = rpc("getnewaddress", ["", "bech32"])
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
                    if lsm and height == 0:
                        assert any(vin.get("txinwitness") for vin in decoded["vin"])
                        target_hash = rpc("getblockhash", [1001])
                        target_txid = decoded["txid"]
                        target_raw = signed["hex"]
                        expected_root = rpc("getblockheader", [target_hash])["merkleroot"]
                    txid, vout, amount = decoded["txid"], 0, output_amount
                    if (height + 1) % 100 == 0:
                        print(f"Generated {height + 1}/600 large regtest blocks", flush=True)

                app = [binary, "--network", "regtest", "--data-dir", str(root / "archive")]
                online = ["--rpc-url", f"http://127.0.0.1:{rpc_port}/", "--cookie-file", str(cookie)]

                def run(*args):
                    result = subprocess.run([*app, *args], capture_output=True, text=True, timeout=240)
                    assert result.returncode == 0, result.stderr
                    return json.loads(result.stdout)

                target_height = 1001 if lsm else 10
                if lsm:
                    sync_port = port()
                    with (root / "lsm-sync.log").open("w") as sync_log:
                        syncing = subprocess.Popen([*app, "lsm-serve", *online, "--listen", f"127.0.0.1:{sync_port}"], stdout=sync_log, stderr=sync_log)
                        client = None
                        try:
                            client = wait(lambda: Electrum(sync_port), timeout=120)
                            wait(lambda: client.request("server.health")["ready"], timeout=600)
                            assert client.request("server.health")["indexed_tip"]["height"] == 1600
                        finally:
                            if client:
                                client.close()
                            syncing.terminate()
                            assert syncing.wait(timeout=150) == 0, (root / "lsm-sync.log").read_text()
                else:
                    assert run("sync", *online, "--max-blocks", "5000")["indexed_tip"]["height"] == 1600
                if automatic:
                    # Stop the generator completely before managed maintenance.
                    rpc("stop")
                    assert node.wait(timeout=60) == 0
                    backups = root / "automatic-backups"
                    backups.mkdir()
                    managed_port = port()
                    with (root / "automatic.log").open("w") as managed_log:
                        managed = subprocess.Popen([
                            *app, "managed-serve", *backend, "--bitcoind", bitcoind,
                            "--core-dir", str(data), "--rpc-port", str(rpc_port),
                            "--listen", f"127.0.0.1:{managed_port}", "--confirm-dedicated", "--offline",
                            "--auto-prune", "--prune-backup-root", str(backups),
                            "--auto-prune-interval-blocks", "288",
                        ], stdout=managed_log, stderr=managed_log)
                        client = None
                        try:
                            wait(lambda: "Poda automática completada:" in (root / "automatic.log").read_text(), timeout=600)
                            client = wait(lambda: Electrum(managed_port), timeout=120)
                            wait(lambda: client.request("server.health")["ready"], timeout=120)
                            assert client.request("blockchain.transaction.get", [target_txid]) == target_raw
                            branch = client.request("blockchain.transaction.get_merkle", [target_txid, target_height])
                            assert proof(target_txid, branch["merkle"], branch["pos"]) == expected_root
                            assert rpc("getblockchaininfo")["pruneheight"] > target_height
                            try:
                                rpc("getblock", [target_hash, 0])
                            except RuntimeError as error:
                                assert "pruned" in str(error)
                            else:
                                raise AssertionError("automatic prune did not delete historical Core bytes")
                            manifest_glob = "lsm-prune-*/backup/generation.json" if lsm else "auto-prune-*/backup/BACKUP_COMPLETE.json"
                            manifests = list(backups.glob(manifest_glob))
                            assert len(manifests) == 1, manifests
                        finally:
                            if client:
                                client.close()
                            managed.terminate()
                            assert managed.wait(timeout=150) == 0, (root / "automatic.log").read_text()
                    # Restart verifies the persisted scheduling cursor: no new
                    # backup or prune request for the same durable height.
                    with (root / "automatic-restart.log").open("w") as restart_log:
                        restarted = subprocess.Popen(managed.args, stdout=restart_log, stderr=restart_log)
                        client = None
                        try:
                            client = wait(lambda: Electrum(managed_port), timeout=120)
                            wait(lambda: client.request("server.health")["ready"], timeout=120)
                            time.sleep(3)
                            assert len(list(backups.glob(manifest_glob))) == 1
                        finally:
                            if client:
                                client.close()
                            restarted.terminate()
                            assert restarted.wait(timeout=150) == 0, (root / "automatic-restart.log").read_text()
                    print("PASS AUTOMATIC PRUNING: owned supervisor, verified backup, real Core deletion, local witness TX/Merkle, restart cursor", flush=True)
                    if lsm:
                        # Fault injection only in this disposable owned generation:
                        # retain the durable request, omit its completion record.
                        journal = root / "archive/regtest/prune-audit.jsonl"
                        records = journal.read_bytes().splitlines(keepends=True)
                        assert len(records) == 2 and json.loads(records[0])["actual"] is None
                        journal.write_bytes(records[0])
                        with journal.open("rb") as file:
                            os.fsync(file.fileno())
                        assert not cookie.exists()
                        rejected = subprocess.run(managed.args, capture_output=True, text=True, timeout=150)
                        assert rejected.returncode != 0 and "unfinished LSM prune audit" in rejected.stderr
                        assert not cookie.exists(), "Core was started despite unfinished prune audit"
                        assert journal.read_bytes() == records[0]
                        assert len(list(backups.glob(manifest_glob))) == 1
                        print("PASS LSM unfinished prune intent blocks supervisor before Core startup; journal and backup preserved", flush=True)
                    return
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
                if node.poll() is None:
                    try:
                        rpc("stop")
                        node.wait(timeout=30)
                    except (OSError, RuntimeError, subprocess.TimeoutExpired):
                        node.kill()
                        node.wait()


if __name__ == "__main__":
    main()

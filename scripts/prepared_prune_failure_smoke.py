#!/usr/bin/env python3
"""Fail-closed prepared prune checks on an offline disposable regtest."""
import argparse
import http.server
import json
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bitcoind", required=True)
    parser.add_argument("--bitcoin-cli", required=True)
    parser.add_argument("--electrum-lite", required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="prepared-prune-", dir="/tmp/opencode") as tmp:
        root = Path(tmp)
        core = root / "core"
        core.mkdir()
        rpc = free_port()
        node = subprocess.Popen([args.bitcoind, "-regtest", f"-datadir={core}", f"-rpcport={rpc}", "-server=1", "-listen=0", "-connect=0", "-dnsseed=0", "-prune=1"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        def cli(*command):
            result = subprocess.run([args.bitcoin_cli, "-regtest", f"-datadir={core}", f"-rpcport={rpc}", *map(str, command)], capture_output=True, text=True, timeout=30, check=True).stdout.strip()
            try:
                return json.loads(result)
            except json.JSONDecodeError:
                return result

        command = [str(Path(args.electrum_lite).resolve()), "--network", "regtest", "--data-dir", str(root / "lite"), "--core-cookie", str(core / "regtest/.cookie"), "--core-rpc", f"http://127.0.0.1:{rpc}"]

        def lite(*args):
            return subprocess.run(command + list(map(str, args)), capture_output=True, text=True, timeout=90)

        try:
            deadline = time.monotonic() + 30
            while True:
                try:
                    cli("getblockcount")
                    break
                except subprocess.CalledProcessError:
                    if time.monotonic() >= deadline or node.poll() is not None:
                        raise
                    time.sleep(0.1)
            cli("createwallet", "miner")
            address = cli("getnewaddress")
            cli("generatetoaddress", 999, address)
            result = lite("sync")
            assert result.returncode == 0, result.stderr
            maintenance = root / "lite/regtest/supervisor"
            private = maintenance / "c-test"
            private.mkdir(parents=True)
            snapshot = maintenance / "snapshot-999-1"
            audit = maintenance / "audit-999-1"
            result = lite("--backup-dir", snapshot, "backup")
            assert result.returncode == 0, result.stderr
            shutil.copytree(snapshot, audit)
            cli("generatetoaddress", 1, address)
            result = lite("sync")
            assert result.returncode == 0, result.stderr
            cli("setnetworkactive", "false")
            before = cli("getblockchaininfo")["pruneheight"]
            proof = private / "audit-1.ok"
            proof.write_text("invalid proof\n")
            backup = maintenance / "backup-999-1"
            shutil.copytree(snapshot, backup)
            prune = ["--confirm-prune", "--prepared-snapshot", snapshot, "--prepared-proof", proof, "--audit-dir", audit, "--backup-dir", backup, "prune"]
            rejected = lite(*prune)
            assert rejected.returncode != 0 and "prepared proof mismatch" in rejected.stderr, rejected.stderr
            assert backup.exists() and not (root / "lite/regtest/prune-pending").exists()
            proof.write_text(f"snapshot={snapshot.resolve()}\naudit={audit.resolve()}\nbackup={backup.resolve()}\n")
            rejected = lite(*prune)
            assert rejected.returncode != 0 and "snapshot tip differs" in rejected.stderr, rejected.stderr
            assert backup.exists() and not (root / "lite/regtest/prune-pending").exists()
            assert cli("getblockchaininfo")["pruneheight"] == before
            assert snapshot.exists() and audit.exists() and proof.exists()
            new_snapshot = maintenance / "snapshot-1000-2"
            new_audit = maintenance / "audit-1000-2"
            stale_backup = maintenance / "backup-1000-2"
            result = lite("--backup-dir", new_snapshot, "backup")
            assert result.returncode == 0, result.stderr
            result = lite("--snapshot-dir", new_snapshot, "--audit-dir", new_audit, "audit")
            assert result.returncode == 0, result.stderr
            shutil.copytree(snapshot, stale_backup)
            new_proof = private / "audit-2.ok"
            new_proof.write_text(f"snapshot={new_snapshot.resolve()}\naudit={new_audit.resolve()}\nbackup={stale_backup.resolve()}\n")
            rejected = lite("--confirm-prune", "--prepared-snapshot", new_snapshot, "--prepared-proof", new_proof, "--audit-dir", new_audit, "--backup-dir", stale_backup, "prune")
            assert rejected.returncode != 0 and "snapshot tip differs" in rejected.stderr, rejected.stderr
            assert cli("getblockchaininfo")["pruneheight"] == before and not (root / "lite/regtest/prune-pending").exists()
            assert stale_backup.exists() and new_snapshot.exists() and new_audit.exists()
            print("PASS: invalid proof, stale snapshot and stale backup rejected before prune; generations preserved")
            # Exercise the private in-server pruning path without touching any
            # existing node. Tiny regtest blocks need not cause physical deletion.
            live_snapshot = maintenance / "snapshot-1000-3"
            live_audit = maintenance / "audit-1000-3"
            live_backup = maintenance / "backup-1000-3"
            shutil.copytree(new_snapshot, live_snapshot)
            shutil.copytree(new_audit, live_audit)
            result = lite("--snapshot-dir", live_snapshot, "--backup-dir", live_backup, "backup")
            assert result.returncode == 0, result.stderr
            receipt = private / "audit-3.ok"
            receipt.write_text(f"snapshot={live_snapshot.resolve()}\naudit={live_audit.resolve()}\nbackup={live_backup.resolve()}\n")
            private.chmod(0o700)
            control = private / "s"
            listen = free_port()
            server = subprocess.Popen(command + ["--listen", f"127.0.0.1:{listen}", "--control-socket", str(control), "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            def request(value):
                with socket.socket(socket.AF_UNIX) as sock:
                    sock.settimeout(15)
                    sock.connect(str(control))
                    sock.sendall(json.dumps(value).encode() + b"\n")
                    return json.loads(sock.makefile("rb").readline())
            def ping():
                with socket.create_connection(("127.0.0.1", listen), timeout=5) as sock:
                    sock.sendall(b'{"id":1,"method":"server.ping","params":[]}\n')
                    return json.loads(sock.makefile("rb").readline())["result"]
            try:
                deadline = time.monotonic() + 30
                while True:
                    try:
                        response = request({"method":"verify-prepared", "snapshot":str(live_snapshot), "audit":str(live_audit), "backup":str(live_backup)})
                        if response.get("result", {}).get("state") == "running":
                            break
                    except (OSError, ValueError):
                        pass
                    if server.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("private verification startup failed")
                    time.sleep(0.1)
                while True:
                    state = request({"method":"verification-status"})["result"]
                    if state["state"] == "verified":
                        break
                    assert state["state"] == "running", state
                    if time.monotonic() >= deadline:
                        raise RuntimeError("verification timed out")
                    time.sleep(0.1)
                invalid = request({"method":"prune-prepared", "proof":str(proof)})
                assert "prepared proof mismatch" in invalid.get("error", {}).get("message", ""), invalid
                assert not (root / "lite/regtest/prune-pending").exists()
                started = request({"method":"prune-prepared", "proof":str(receipt)})
                assert started.get("result", {}).get("state") == "pruning", started
                assert ping() is None
                while True:
                    state = request({"method":"verification-status"})["result"]
                    if state["state"] == "pruned":
                        break
                    assert state["state"] == "pruning", state
                    if time.monotonic() >= deadline:
                        raise RuntimeError("prune timed out")
                    time.sleep(0.1)
                assert (root / "lite/regtest/prune-complete-1000").is_file()
                assert not (root / "lite/regtest/prune-pending").exists()
                print("PASS: private prune rejected bad proof, completed with server running and kept durable record")
            finally:
                if server.poll() is None:
                    server.terminate()
                    server.wait(timeout=30)
            # Reject the actual pruning RPC after durable intent has been
            # written. The failed server must stay fail-closed and preserve it.
            fail_root = root / "failed-lite/regtest"
            (fail_root / "supervisor/c-test").mkdir(parents=True, mode=0o700)
            shutil.copytree(root / "lite/regtest/index", fail_root / "index")
            fail_dir = fail_root / "supervisor"
            fail_snapshot = fail_dir / "snapshot-1000-4"
            fail_audit = fail_dir / "audit-1000-4"
            fail_backup = fail_dir / "backup-1000-4"
            shutil.copytree(live_snapshot, fail_snapshot)
            shutil.copytree(live_audit, fail_audit)
            shutil.copytree(live_backup, fail_backup)
            fail_receipt = fail_dir / "c-test/audit-4.ok"
            fail_receipt.write_text(f"snapshot={fail_snapshot.resolve()}\naudit={fail_audit.resolve()}\nbackup={fail_backup.resolve()}\n")
            class FailureProxy(http.server.BaseHTTPRequestHandler):
                def log_message(self, *unused):
                    pass
                def do_POST(self):
                    data = self.rfile.read(int(self.headers["Content-Length"]))
                    message = json.loads(data)
                    if isinstance(message, dict) and message.get("method") == "pruneblockchain":
                        body = json.dumps({"id":message["id"],"result":None,"error":{"code":-1,"message":"injected failure"}}).encode()
                    else:
                        forwarded = urllib.request.Request(f"http://127.0.0.1:{rpc}", data, {"Authorization": self.headers["Authorization"]})
                        try:
                            response = urllib.request.urlopen(forwarded, timeout=30)
                        except urllib.error.HTTPError as error:
                            response = error
                        with response:
                            body = response.read()
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
            proxy = http.server.ThreadingHTTPServer(("127.0.0.1", free_port()), FailureProxy)
            threading.Thread(target=proxy.serve_forever, daemon=True).start()
            control = fail_dir / "c-test/s"
            listen = free_port()
            fail_command = command.copy()
            fail_command[fail_command.index("--data-dir") + 1] = str(root / "failed-lite")
            fail_command[fail_command.index("--core-rpc") + 1] = f"http://127.0.0.1:{proxy.server_port}"
            server = subprocess.Popen(fail_command + ["--listen", f"127.0.0.1:{listen}", "--control-socket", str(control), "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 30
                while True:
                    try:
                        response = request({"method":"verify-prepared", "snapshot":str(fail_snapshot), "audit":str(fail_audit), "backup":str(fail_backup)})
                        if response.get("result", {}).get("state") == "running":
                            break
                    except (OSError, ValueError):
                        pass
                    if server.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("failed-prune verification startup failed")
                    time.sleep(0.1)
                while request({"method":"verification-status"})["result"]["state"] == "running":
                    assert time.monotonic() < deadline
                    time.sleep(0.1)
                assert request({"method":"verification-status"})["result"]["state"] == "verified"
                assert request({"method":"prune-prepared", "proof":str(fail_receipt)})["result"]["state"] == "pruning"
                while True:
                    state = request({"method":"verification-status"})["result"]
                    if state["state"] == "prune-failed":
                        break
                    assert state["state"] == "pruning" and time.monotonic() < deadline, state
                    time.sleep(0.1)
                assert (fail_root / "prune-pending").is_file()
                assert not (fail_root / "prune-complete-1000").exists()
                assert request({"method":"release-verification"}).get("error")
                assert ping() is None
                with socket.create_connection(("127.0.0.1", listen), timeout=5) as sock:
                    sock.sendall(b'{"id":2,"method":"blockchain.headers.subscribe","params":[]}\n')
                    assert json.loads(sock.makefile("rb").readline()).get("error")
                print("PASS: failed prune RPC preserves intent and blocks chain queries and lease release")
            finally:
                if server.poll() is None:
                    server.terminate()
                    server.wait(timeout=30)
                proxy.shutdown()
        finally:
            if node.poll() is None:
                node.terminate()
                node.wait(timeout=30)


if __name__ == "__main__":
    main()

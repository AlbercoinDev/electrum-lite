#!/usr/bin/env python3
"""Fail-closed prepared prune checks on an offline disposable regtest."""
import argparse
import json
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time


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
            shutil.copytree(new_snapshot, new_audit)
            shutil.copytree(snapshot, stale_backup)
            new_proof = private / "audit-2.ok"
            new_proof.write_text(f"snapshot={new_snapshot.resolve()}\naudit={new_audit.resolve()}\nbackup={stale_backup.resolve()}\n")
            rejected = lite("--confirm-prune", "--prepared-snapshot", new_snapshot, "--prepared-proof", new_proof, "--audit-dir", new_audit, "--backup-dir", stale_backup, "prune")
            assert rejected.returncode != 0 and "snapshot tip differs" in rejected.stderr, rejected.stderr
            assert cli("getblockchaininfo")["pruneheight"] == before and not (root / "lite/regtest/prune-pending").exists()
            assert stale_backup.exists() and new_snapshot.exists() and new_audit.exists()
            print("PASS: invalid proof, stale snapshot and stale backup rejected before prune; generations preserved")
        finally:
            if node.poll() is None:
                node.terminate()
                node.wait(timeout=30)


if __name__ == "__main__":
    main()

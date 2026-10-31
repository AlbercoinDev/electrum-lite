#!/usr/bin/env python3
"""Real deletion test on TWO disposable offline regtests (reserve 3 GiB temporary).

One Core is created/owned by managed-serve, the other is an archival P2P source.
Nothing targets an existing node. No public peers, no external APIs.
"""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import socket
import shutil
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait(fn, label, timeout=180, guard=None):
    until = time.monotonic() + timeout
    last = None
    while time.monotonic() < until:
        if guard is not None and guard.poll() is not None:
            raise RuntimeError(f"owned process exited while waiting for {label}: {guard.returncode}")
        try:
            value = fn()
            if value:
                return value
        except Exception as e:
            last = e
        time.sleep(0.15)
    raise RuntimeError(f"timeout: {label}: {last}")


class RPC:
    def __init__(self, cookie, rpcport):
        self.cookie, self.port = cookie, rpcport

    def __call__(self, method, *params):
        body = json.dumps({"id": 1, "method": method, "params": params}).encode()
        token = base64.b64encode(self.cookie.read_bytes().strip()).decode()
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}", body, {"Authorization": "Basic " + token})
        try:
            response = urllib.request.urlopen(request, timeout=90)
        except urllib.error.HTTPError as e:
            response = e
        result = json.loads(response.read())
        if result.get("error"):
            raise RuntimeError(result["error"])
        return result["result"]


def query(listen, method, *params):
    with socket.create_connection(("127.0.0.1", listen), timeout=10) as s:
        s.settimeout(90)
        s.sendall(json.dumps({"id": 1, "method": method, "params": params}).encode() + b"\n")
        r = json.loads(s.makefile("rb").readline(16_200_001))
        if "error" in r:
            raise RuntimeError(r["error"])
        return r["result"]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--bitcoind", default="bitcoind")
    p.add_argument("--electrum-lite", default="target/release/electrum-lite")
    p.add_argument("--temp-dir",type=Path,help="Temporary parent on a disk with >=3 GiB free (not necessarily tmpfs)")
    a = p.parse_args()
    binary = str(Path(a.electrum_lite).resolve())
    base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    if a.temp_dir is not None:
        a.temp_dir.mkdir(parents=True,exist_ok=True)
        base=str(a.temp_dir.resolve())
    with tempfile.TemporaryDirectory(prefix="el-managed-prune-", dir=base) as tmp:
        root = Path(tmp)
        assert shutil.disk_usage(root).free>=3*1024**3,"physical-prune smoke requires at least 3 GiB free temporary space"
        processes, files = [], []
        coreport, corep2p, sourceport, sourcep2p, listen = [port() for _ in range(5)]
        dedicated, lite, source = root / "dedicated", root / "lite", root / "source"
        source.mkdir()
        def launch(command, name):
            f = open(root / (name + ".log"), "w+")
            files.append(f)
            child = subprocess.Popen(command, stdout=f, stderr=f)
            processes.append(child)
            return child
        def stop(child):
            child.terminate()
            assert child.wait(timeout=90) == 0, "unclean owned shutdown"
        command = [binary, "--network", "regtest", "--data-dir", str(lite), "--core-dir", str(dedicated), "--bitcoind", a.bitcoind, "--core-rpc", f"http://127.0.0.1:{coreport}", "--core-p2p-port", str(corep2p), "--listen", f"127.0.0.1:{listen}", "--confirm-dedicated", "--pause-lag", "200", "--resume-lag", "20", "--block-peer", f"127.0.0.1:{sourcep2p}"]
        rpc = RPC(dedicated / "regtest/.cookie", coreport)
        archive = RPC(source / "regtest/.cookie", sourceport)
        events = lite / "regtest/supervisor/events.jsonl"
        def has_event(name):
            return events.exists() and any(json.loads(line)["event"] == name for line in events.read_text().splitlines())
        try:
            launch([a.bitcoind, "-regtest", f"-datadir={source}", f"-rpcport={sourceport}", f"-port={sourcep2p}", f"-bind=127.0.0.1:{sourcep2p}", "-server=1", "-listen=1", "-connect=0", "-dnsseed=0", "-discover=0", "-listenonion=0", "-dbcache=64"], "archive")
            wait(lambda: archive("getblockcount") == 0, "archival Core startup")
            manager = launch(command + ["managed-serve"], "manager-first")
            wait(lambda: query(listen, "server.health")["ready"], "managed startup",guard=manager)
            duplicate = subprocess.run(command + ["managed-serve"], capture_output=True, text=True, timeout=20)
            assert duplicate.returncode and manager.poll() is None
            assert rpc("getblockcount") == 0
            # Produce a valid but deliberately stale chain so the dedicated
            # Core remains in initialblockdownload even after indexing catches up.
            archive("setmocktime", int(time.time()) - 2 * 24 * 60 * 60)
            archive("createwallet", "miner")
            address = archive("getnewaddress")
            # Core 31's normal regtest requires height >= 1000 for pruning.
            # Do not bypass it with -fastprune: retain normal 128 MiB files.
            archive("generatetoaddress", 601, address)
            coin = archive("listunspent", 100)[0]
            prev, vout, amount = coin["txid"], coin["vout"], round(coin["amount"] * 100_000_000)
            payload = "00" * 950_000
            historical = None
            # Big unspendable outputs: real consensus-valid blocks, no policy relay.
            for n in range(450):
                assert manager.poll() is None, "manager exited during fixture generation"
                amount -= 1000
                raw = archive("createrawtransaction", [{"txid": prev, "vout": vout}], [{address: amount / 100_000_000}, {"data": payload}])
                signed = archive("signrawtransactionwithwallet", raw)
                assert signed["complete"]
                blockhash = archive("generateblock", address, [signed["hex"]])["hash"]
                meta = archive("getblock", blockhash, 1)
                prev, vout = meta["tx"][1], 0
                if historical is None:
                    historical = (prev, signed["hex"], blockhash, meta["height"])
                if n % 100 == 0:
                    print(f"generated {n + 1}/450 near-weight-limit blocks", flush=True)
            tip = archive("getblockcount")
            # Real P2P IBD, not submitblock or direct copying of Core's files.
            rpc("addnode", f"127.0.0.1:{sourcep2p}", "add")
            wait(lambda: has_event("backpressure_pause"), "backpressure pause during P2P IBD", timeout=600,guard=manager)
            wait(lambda: rpc("getblockcount") == tip and query(listen, "server.health")["ready"] and query(listen, "server.health")["indexed_height"] == tip, "P2P IBD and indexing catch up", timeout=600,guard=manager)
            wait(lambda: has_event("backpressure_resume"), "backpressure resume",guard=manager)
            assert rpc("getblockchaininfo")["initialblockdownload"] is True
            before_files = {p.name: p.stat().st_size for p in (dedicated / "regtest/blocks").glob("blk*.dat")}
            stop(manager)
            assert has_event("shutdown")
            print("PASS: owned startup, lag pause/resume and clean managed shutdown", flush=True)
            # Synthetic completed generations from this disposable test only.
            # They exercise cleanup after a genuinely verified new generation.
            maintenance = lite / "regtest/supervisor"
            index = lite / "regtest/index"
            def old_generation(height, stamp, foreign=False):
                audit = maintenance / f"audit-{height}-{stamp}"
                backup = maintenance / f"backup-{height}-{stamp}"
                shutil.copytree(index, audit)
                shutil.copytree(index, backup)
                if foreign:
                    (audit / "user-notes.txt").write_text("preserve")
                (lite / f"regtest/prune-complete-{height}").write_text(
                    f"requested_height={height-288}\nbackup={backup}\naudit={audit}\n"
                    "actual_pruneheight=0\nmissing_block_verified=false\n")
                return audit, backup
            old_audit, old_backup = old_generation(1000, 1)
            foreign_audit, foreign_backup = old_generation(1001, 2, foreign=True)
            orphan = maintenance / "audit-999-3"
            orphan.mkdir()
            (orphan / "keep.txt").write_text("incomplete; preserve")
            # Resume only this owned Core. Mining is now finished: no competing writer.
            manager = launch(command + ["--auto-prune", "managed-serve"], "manager-prune")
            wait(lambda: has_event("maintenance_complete"), "coordinated physical prune", timeout=600,guard=manager)
            wait(lambda: has_event("retention_complete"), "verified retention", timeout=120,guard=manager)
            wait(lambda: query(listen, "server.health")["ready"], "post-maintenance server resume")
            event_names = [json.loads(line)["event"] for line in events.read_text().splitlines()]
            assert event_names.index("audit_preparing") < event_names.index("audit_prepared") < event_names.index("backup_preparing") < event_names.index("backup_prepared") < event_names.index("maintenance_offline") < event_names.index("maintenance_complete")
            wait(lambda: not list(maintenance.glob("snapshot-*")), "temporary checkpoint cleanup", timeout=120, guard=manager)
            print("PASS: managed online audit and backup finished before offline prune; temporary checkpoint cleaned", flush=True)
            assert not old_audit.exists() and not old_backup.exists()
            assert (foreign_audit / "user-notes.txt").read_text() == "preserve" and foreign_backup.exists()
            assert (orphan / "keep.txt").exists()
            print("PASS: retention removed the completed old pair and preserved foreign and incomplete data", flush=True)
            info = rpc("getblockchaininfo")
            assert info["initialblockdownload"] is True, "prune was not exercised during IBD"
            assert info["pruneheight"] > historical[3]
            after_files = {p.name: p.stat().st_size for p in (dedicated / "regtest/blocks").glob("blk*.dat")}
            deleted = before_files.keys() - after_files.keys()
            assert deleted and sum(after_files.values()) < sum(before_files.values())
            try:
                rpc("getblock", historical[2], 0)
            except RuntimeError as e:
                assert "-1" in str(e)
            else:
                raise AssertionError("historical block still available in pruned Core")
            print(f"PASS: physical deletion during IBD {sorted(deleted)}, pruneheight={info['pruneheight']}, blk bytes {sum(before_files.values())} -> {sum(after_files.values())}", flush=True)
            txid, raw, _, height = historical
            assert query(listen, "blockchain.transaction.get", txid) == raw
            assert list((lite / "regtest/cache").glob("*.blk"))
            archive("setnetworkactive", False)
            assert query(listen, "blockchain.transaction.get", txid) == raw
            proof = query(listen, "blockchain.transaction.get_merkle", txid, height)
            value, pos = bytes.fromhex(txid)[::-1], proof["pos"]
            for sibling in proof["merkle"]:
                sibling = bytes.fromhex(sibling)[::-1]
                value = hashlib.sha256(hashlib.sha256(sibling + value if pos & 1 else value + sibling).digest()).digest()
                pos >>= 1
            header = bytes.fromhex(query(listen, "blockchain.block.header", height))
            assert value == header[36:68]
            assert not (lite / "regtest/prune-pending").exists()
            assert list((lite / "regtest/supervisor").glob("backup-*"))
            print("PASS: original witness transaction recovered over P2P after deletion; repeat served from cache with archival P2P disabled; local Merkle proof valid", flush=True)
            # A new block alone must not trigger another expensive audit when
            # no closed file has yet moved outside the 288-block safety window.
            cache_before={p.name for p in (lite / "regtest/cache").glob("*.blk")}
            complete_before=sum(json.loads(line)["event"] == "maintenance_complete" for line in events.read_text().splitlines())
            archive("setnetworkactive", True)
            archive("generatetoaddress", 1, address)
            rpc("addnode", f"127.0.0.1:{sourcep2p}", "onetry")
            wait(lambda: query(listen,"server.health")["indexed_height"] == tip+1,"next block indexed",timeout=180,guard=manager)
            time.sleep(3)
            assert manager.poll() is None
            assert sum(json.loads(line)["event"] == "maintenance_complete" for line in events.read_text().splitlines()) == complete_before
            assert {p.name for p in (lite / "regtest/cache").glob("*.blk")}==cache_before
            print("PASS: no redundant maintenance when the next closed file is ineligible",flush=True)
            # Advance enough for another closed block file to leave the
            # 288-block safety window. The previous real audit/backup is now
            # the provenance-checked base of the next automatic maintenance.
            archive("generatetoaddress", 160, address)
            rpc("addnode", f"127.0.0.1:{sourcep2p}", "onetry")
            def completed_count():
                return sum(json.loads(line)["event"] == "maintenance_complete" for line in events.read_text().splitlines())
            wait(lambda: completed_count() >= complete_before + 1, "second physical prune", timeout=600,guard=manager)
            wait(lambda: sum(json.loads(line)["event"] == "retention_complete" for line in events.read_text().splitlines()) >= 2, "second retention", timeout=120,guard=manager)
            wait(lambda: query(listen,"server.health")["ready"], "second maintenance resume", timeout=180, guard=manager)
            assert "incremental" in (root / "manager-prune.log").read_text().lower()
            assert {p.name for p in (lite / "regtest/cache").glob("*.blk")} == cache_before
            assert (foreign_audit / "user-notes.txt").exists() and (orphan / "keep.txt").exists()
            print("PASS: second physical prune used the verified incremental audit without growing the historical cache", flush=True)
            stop(manager)
            # An unfinished intent must prevent starting automatic mode or its Core.
            pending = lite / "regtest/prune-pending"
            pending.write_text("fault-injection: unfinished prune\n")
            rejected = subprocess.run(command + ["--auto-prune", "managed-serve"], capture_output=True, text=True, timeout=20)
            assert rejected.returncode and "unfinished prune" in rejected.stderr
            assert not (dedicated / "electrum-lite.pid").exists()
            print("PASS: pending prune blocks automatic restart; no existing node used", flush=True)
        except Exception:
            for f in files:
                f.flush()
                f.seek(0)
                print(f"--- {f.name} ---\n{f.read()[-5000:]}")
            raise
        finally:
            for child in reversed(processes):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=90)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            for f in files:
                f.close()


if __name__ == "__main__":
    main()

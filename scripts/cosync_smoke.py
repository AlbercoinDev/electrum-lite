#!/usr/bin/env python3
"""Bounded public-network co-sync measurement; OWN disposable nodes only.

No pruning, P2P pauses or maintenance during the measured interval. Retains
JSONL samples and logs. A successful prefix is not a full-network validation.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import shutil
import subprocess
import time
import urllib.error
import urllib.request


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def health(port_number):
    with socket.create_connection(("127.0.0.1", port_number), timeout=5) as stream:
        stream.settimeout(15)
        stream.sendall(b'{"id":1,"method":"server.health","params":[]}\n')
        result = json.loads(stream.makefile("rb").readline(65536))
    if result.get("error"):
        raise RuntimeError(str(result["error"]))
    return result["result"]


def summarize(samples, minimum_seconds):
    valid = [s for s in samples if "lite_height" in s and s.get("last_error") is None]
    # Conservative bounds: Core is sampled before AND after the health call.
    # Require separated endpoints with backlog, sustained time and active Core.
    candidates = []
    for i, first in enumerate(valid):
        for last in valid[i + 1:]:
            duration = last["elapsed"] - first["elapsed"]
            if duration < minimum_seconds:
                continue
            inside = [s for s in valid if first["elapsed"] <= s["elapsed"] <= last["elapsed"]]
            moving = [(a, b) for a, b in zip(inside, inside[1:])
                      if b["core_before"] > a["core_after"] and b["elapsed"] - a["elapsed"] <= 15]
            active = len(moving)
            active_seconds = sum(b["elapsed"] - a["elapsed"] for a, b in moving)
            active_reduction = sum(a["lag_lower"] - b["lag_upper"] for a, b in moving)
            reduction = first["lag_lower"] - last["lag_upper"]
            if (reduction > 0 and last["core_before"] - first["core_after"] >= 1000
                    and active >= 10 and active_seconds >= minimum_seconds
                    and active_seconds >= 0.8 * duration and active_reduction > 0):
                candidates.append({"seconds": duration, "lag_reduction_at_least": reduction,
                                   "core_advance_at_least": last["core_before"] - first["core_after"],
                                   "active_sample_intervals": active,
                                   "active_seconds": active_seconds,
                                   "net_active_lag_reduction": active_reduction,
                                   "from": first["elapsed"], "to": last["elapsed"]})
    return {"result": "BOUNDED_COSYNC_LAG_REDUCTION" if candidates else "NOT_DEMONSTRATED",
            "qualifying_window": max(candidates, key=lambda c: c["seconds"]) if candidates else None,
            "valid_samples": len(valid), "full_sync": False,
            "maintenance_seconds": 0, "p2p_pauses_during_measurement": 0}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--network", choices=["signet", "testnet4"], default="signet")
    parser.add_argument("--root", type=Path, required=True, help="NEW directory; retained")
    parser.add_argument("--seconds", type=int, default=600)
    parser.add_argument("--max-download-mib", type=int, default=512)
    parser.add_argument("--prefill-blocks", type=int, default=4096)
    parser.add_argument("--minimum-window-seconds", type=int, default=120)
    args = parser.parse_args()
    if not (120 <= args.seconds <= 1800 and 1 <= args.max_download_mib <= 2048
            and args.prefill_blocks >= 1 and args.minimum_window_seconds >= 120):
        parser.error("invalid bounded measurement budgets")
    args.root = args.root.absolute()
    args.root.mkdir()  # Never adopt existing/user nodes.
    if shutil.disk_usage(args.root).free < 1024 * 1024 * 1024:
        raise RuntimeError("less than 1 GiB free; no node started")
    data = args.root / "core"
    data.mkdir()
    binary = Path(os.environ.get("ELECTRUM_LITE", "target/release/electrum-lite")).resolve()
    bitcoind = os.environ.get("BITCOIND", "bitcoind")
    rpc_port, electrum_port = port(), port()
    cookie = data / args.network / ".cookie"

    def rpc(method, params=None):
        auth = base64.b64encode(cookie.read_bytes().strip()).decode()
        request = urllib.request.Request(f"http://127.0.0.1:{rpc_port}/",
            data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode(),
            headers={"Authorization": "Basic " + auth, "Content-Type": "application/json"})
        try:
            response = urllib.request.urlopen(request, timeout=15)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            result = json.load(response)
        if result.get("error"):
            raise RuntimeError(str(result["error"]))
        return result["result"]

    metadata = {"network": args.network, "binary": str(binary),
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "core_version": subprocess.check_output([bitcoind, "--version"], text=True).splitlines()[0],
                "budgets": vars(args) | {"root": str(args.root)}, "started_unix": time.time()}
    (args.root / "metadata.json").write_text(json.dumps(metadata, indent=2))
    samples = []
    app = None
    with (args.root / "core.log").open("w") as core_log, (args.root / "lite.log").open("w") as lite_log:
        core = subprocess.Popen([bitcoind, f"-{args.network}", f"-datadir={data}", f"-rpcport={rpc_port}",
            "-server=1", "-listen=0", "-maxconnections=8", "-blocksonly=1", "-dbcache=64",
            "-prune=1", "-txindex=0", "-rpcbind=127.0.0.1", "-rpcallowip=127.0.0.1"],
            stdout=core_log, stderr=core_log)
        started = time.monotonic()
        measuring = None
        abort_reason = None
        try:
            with (args.root / "samples.jsonl").open("w", buffering=1) as output:
                while time.monotonic() - started < args.seconds:
                    if shutil.disk_usage(args.root).free < 768 * 1024 * 1024:
                        raise RuntimeError("free-space guard: measurement aborted before Core disk exhaustion")
                    if core.poll() is not None or (app is not None and app.poll() is not None):
                        raise RuntimeError("owned child exited; inspect retained logs")
                    try:
                        before = rpc("getblockchaininfo")
                        received = rpc("getnettotals")["totalbytesrecv"]
                        peers = rpc("getnetworkinfo")["connections"]
                        if received >= args.max_download_mib * 1024 * 1024:
                            break
                        if app is None and before["blocks"] >= args.prefill_blocks:
                            app = subprocess.Popen([str(binary), "--network", args.network,
                                "--data-dir", str(args.root / "lite"), "serve", "--rpc-url",
                                f"http://127.0.0.1:{rpc_port}/", "--cookie-file", str(cookie),
                                "--listen", f"127.0.0.1:{electrum_port}"], stdout=lite_log, stderr=lite_log)
                            measuring = time.monotonic()
                        sample = {"elapsed": time.monotonic() - started, "core_before": before["blocks"],
                                  "headers": before["headers"], "received_bytes": received, "peers": peers,
                                  "free_bytes": shutil.disk_usage(args.root).free}
                        if app is not None:
                            status = health(electrum_port)
                            after = rpc("getblockchaininfo")
                            height = status["indexed_tip"]["height"]
                            sample.update({"core_after": after["blocks"], "lite_height": height,
                                "lag_lower": max(0, before["blocks"] - height),
                                "lag_upper": max(0, after["blocks"] - height),
                                "sync_metrics": status["sync_metrics"], "last_error": status["last_error"]})
                        samples.append(sample)
                        output.write(json.dumps(sample) + "\n")
                        print(json.dumps(sample), flush=True)
                    except (OSError, RuntimeError, TypeError, KeyError) as error:
                        output.write(json.dumps({"elapsed": time.monotonic() - started, "error": str(error)}) + "\n")
                    time.sleep(2)
        except RuntimeError as error:
            abort_reason = str(error)
        finally:
            # Shutdown is outside the measured interval and never counted as progress.
            if app is not None:
                app.terminate()
                try:
                    app.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
            try:
                rpc("stop")
                core.wait(timeout=120)
            except (OSError, RuntimeError, subprocess.TimeoutExpired):
                core.kill()
                core.wait()
    result = summarize(samples, args.minimum_window_seconds)
    result["abort_reason"] = abort_reason
    if abort_reason:
        result["result"] = "NOT_DEMONSTRATED"
    result["measurement_started"] = measuring is not None
    result["elapsed_seconds"] = time.monotonic() - started
    (args.root / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2), flush=True)
    return 0 if result["result"] == "BOUNDED_COSYNC_LAG_REDUCTION" else 2


if __name__ == "__main__":
    raise SystemExit(main())

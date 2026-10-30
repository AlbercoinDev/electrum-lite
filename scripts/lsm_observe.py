#!/usr/bin/env python3
"""Read-only bounded observations of an installed LSM service; never starts nodes."""
import argparse
import base64
import json
import os
from pathlib import Path
import socket
import time
import urllib.request


def disk_bytes(path, maximum=1_000_000):
    total = allocated = count = 0
    if not path.exists():
        return {"logical_bytes": 0, "allocated_bytes": 0, "files": 0}
    for directory, directories, files in os.walk(path, followlinks=False):
        directories[:] = [name for name in directories if not (Path(directory) / name).is_symlink()]
        for name in files:
            target = Path(directory) / name
            if target.is_symlink():
                continue
            stat = target.stat()  # concurrent deletion fails this sample, never silently undercounts
            count += 1
            if count > maximum:
                raise RuntimeError("disk observation file budget exceeded")
            total += stat.st_size
            allocated += stat.st_blocks * 512
    return {"logical_bytes": total, "allocated_bytes": allocated, "files": count}


def health():
    with socket.create_connection(("127.0.0.1", 60601), timeout=3) as stream:
        stream.settimeout(3)
        stream.sendall(b'{"id":1,"method":"server.health","params":[]}\n')
        with stream.makefile("rb") as reader:
            value = json.loads(reader.readline(65536))
    result = value["result"]
    if result.get("backend") != "lsm" or result.get("network") != "signet":
        raise RuntimeError("not the Signet LSM service")
    return result


def core_info(cookie):
    credentials = cookie.read_bytes().strip()
    if not credentials or len(credentials) > 4096:
        raise RuntimeError("invalid cookie")
    request = urllib.request.Request("http://127.0.0.1:39332/",
        data=b'{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}',
        headers={"Authorization": "Basic " + base64.b64encode(credentials).decode(),
                 "Content-Type": "application/json"})
    # Explicitly bypass environment proxies for the local authenticated request.
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=5) as response:
        info = json.loads(response.read(65536))
    if info.get("error") or info["result"].get("chain") != "signet":
        raise RuntimeError("Core Signet observation failed")
    return info["result"]


def memory(root):
    """Only executables inside this bundle; no command lines or credentials logged."""
    result = {"lite_rss_bytes": 0, "core_rss_bytes": 0, "processes": 0}
    versions = (root / "versions").resolve()
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            executable = (entry / "exe").resolve(strict=True)
            if not executable.is_relative_to(versions):
                continue
            family = "core" if executable.name == "bitcoind" else "lite" if executable.name == "electrum-lite" else None
            if family is None:
                continue
            fields = (entry / "statm").read_text().split()
            result[family + "_rss_bytes"] += int(fields[1]) * os.sysconf("SC_PAGE_SIZE")
            result["processes"] += 1
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
    result["total_rss_bytes"] = result["lite_rss_bytes"] + result["core_rss_bytes"]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.home() / "electrum-lite")
    parser.add_argument("--output", type=Path, required=True, help="NEW JSONL destination")
    parser.add_argument("--seconds", type=int, default=300)
    parser.add_argument("--interval", type=int, default=10)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 86400 or not 5 <= args.interval <= 3600:
        parser.error("duration 1..86400 s; interval 5..3600 s")
    root = args.root.expanduser().resolve()
    generation = root / "data/signet/lsm-v3"
    started = time.monotonic()
    with args.output.open("x") as output:
        while True:
            sample = {"elapsed_seconds": round(time.monotonic() - started, 3), "unix_time": time.time()}
            try:
                status = health()
                info = core_info(generation / "core/signet/.cookie")
                indexed = (status.get("indexed_tip") or {}).get("height")
                sample.update(core_height=info["blocks"], core_headers=info["headers"],
                    core_ibd=info["initialblockdownload"], core_verificationprogress=info["verificationprogress"],
                    indexed_height=indexed, lag_blocks=None if indexed is None else info["blocks"] - indexed,
                    ready=status["ready"], memory=memory(root),
                    core_disk=disk_bytes(generation / "core"), lite_disk=disk_bytes(generation / "lite"),
                    backups_disk=disk_bytes(root / "backups/signet"),
                    bundle_disk=disk_bytes(root))
            except Exception:
                # Never persist RPC bodies, cookies, scripts or wallet identifiers.
                sample["error"] = "observation unavailable; sample is not evidence of zero usage or zero lag"
            output.write(json.dumps(sample, sort_keys=True) + "\n")
            output.flush()
            remaining = args.seconds - (time.monotonic() - started)
            if remaining <= 0:
                break
            time.sleep(min(args.interval, remaining))
    print("Observations saved. No nodes started, stopped, paused or pruned.")


if __name__ == "__main__":
    main()

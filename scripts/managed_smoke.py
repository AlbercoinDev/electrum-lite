#!/usr/bin/env python3
"""Linux-only supervisor test: own offline regtest Core, never an existing node."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def children(pid):
    path = Path(f"/proc/{pid}/task/{pid}/children")
    return [int(value) for value in path.read_text().split()] if path.exists() else []


def running(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][0] != "Z"
    except FileNotFoundError:
        return False


def main():
    binary = str(Path(os.environ.get("ELECTRUM_LITE", "target/release/electrum-lite")).resolve())
    bitcoind = str(Path(os.environ.get("BITCOIND", "bitcoind")).resolve())
    bitcoincli = str(Path(os.environ.get("BITCOINCLI", "bitcoin-cli")).resolve())
    with tempfile.TemporaryDirectory(prefix="electrum-managed-") as temporary:
        root = Path(temporary)
        core_dir = root / "dedicated-core"
        rpc_port, electrum_port = port(), port()
        command = [binary, "--network", "regtest", "--data-dir", str(root / "archive"),
                   "managed-serve", "--bitcoind", bitcoind, "--core-dir", str(core_dir),
                   "--rpc-port", str(rpc_port), "--listen", f"127.0.0.1:{electrum_port}",
                   "--confirm-dedicated", "--offline"]
        with (root / "supervisor.log").open("w") as log:
            supervisor = subprocess.Popen(command, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 90
                while True:
                    assert supervisor.poll() is None, (root / "supervisor.log").read_text()
                    found = {}
                    for pid in children(supervisor.pid):
                        try:
                            cmd = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                            found[Path(os.fsdecode(cmd[0])).name] = pid
                        except FileNotFoundError:
                            continue
                    if "electrum-lite" in found and "bitcoind" in found:
                        core_pid, app_pid = found["bitcoind"], found["electrum-lite"]
                        result = subprocess.run([bitcoincli, "-regtest", f"-datadir={core_dir}", f"-rpcport={rpc_port}", "getblockchaininfo"], capture_output=True, text=True)
                        if result.returncode == 0:
                            info = json.loads(result.stdout)
                            assert info["pruned"] and not info["automatic_pruning"]
                            break
                    assert time.monotonic() < deadline, (root / "supervisor.log").read_text()
                    time.sleep(.1)
                os.kill(app_pid, signal.SIGKILL)  # Inject a daemon crash; Core belongs only to this test.
                assert supervisor.wait(timeout=150) != 0
                assert not running(core_pid), "owned Core still running after daemon crash"
                text = (root / "supervisor.log").read_text()
                assert "Electrum Lite terminó" in text and "parada confirmada" in text, text
                print("PASS managed Core: daemon crash stops the owned offline test node")
            finally:
                if supervisor.poll() is None:
                    supervisor.terminate()
                    try:
                        supervisor.wait(timeout=150)
                    except subprocess.TimeoutExpired:
                        supervisor.kill()
                        supervisor.wait()
                        raise

        # The Linux parent-death signal also protects these own child processes
        # if the supervisor itself is killed unexpectedly.
        core_dir = root / "dedicated-core-parent-death"
        rpc_port, electrum_port = port(), port()
        command = [binary, "--network", "regtest", "--data-dir", str(root / "archive-parent-death"),
                   "managed-serve", "--bitcoind", bitcoind, "--core-dir", str(core_dir),
                   "--rpc-port", str(rpc_port), "--listen", f"127.0.0.1:{electrum_port}",
                   "--confirm-dedicated", "--offline"]
        with (root / "parent-death.log").open("w") as log:
            supervisor = subprocess.Popen(command, stdout=log, stderr=log)
            owned = []
            try:
                deadline = time.monotonic() + 90
                while True:
                    assert supervisor.poll() is None, (root / "parent-death.log").read_text()
                    owned = children(supervisor.pid)
                    if len(owned) == 2:
                        result = subprocess.run([bitcoincli, "-regtest", f"-datadir={core_dir}", f"-rpcport={rpc_port}", "getblockchaininfo"], capture_output=True, text=True)
                        if result.returncode == 0:
                            break
                    assert time.monotonic() < deadline, (root / "parent-death.log").read_text()
                    time.sleep(.1)
                supervisor.kill()
                supervisor.wait(timeout=10)
                deadline = time.monotonic() + 120
                while any(running(pid) for pid in owned) and time.monotonic() < deadline:
                    time.sleep(.1)
                assert not any(running(pid) for pid in owned), "owned child still running after supervisor death"
                print("PASS managed Core: parent death signals both owned children")
            finally:
                if supervisor.poll() is None:
                    supervisor.terminate()
                    supervisor.wait(timeout=150)
                for pid in owned:
                    try:
                        cmd = Path(f"/proc/{pid}/cmdline").read_bytes()
                        if os.fsencode(str(root)) in cmd:
                            os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    except FileNotFoundError:
                        pass

        # A frozen daemon cannot answer health probes; the Core must stop first.
        core_dir = root / "dedicated-core-stall"
        rpc_port, electrum_port = port(), port()
        command = [binary, "--network", "regtest", "--data-dir", str(root / "archive-stall"),
                   "managed-serve", "--bitcoind", bitcoind, "--core-dir", str(core_dir),
                   "--rpc-port", str(rpc_port), "--listen", f"127.0.0.1:{electrum_port}",
                   "--confirm-dedicated", "--offline"]
        with (root / "stall.log").open("w") as log:
            supervisor = subprocess.Popen(command, stdout=log, stderr=log)
            app_pid = None
            try:
                deadline = time.monotonic() + 90
                while True:
                    assert supervisor.poll() is None, (root / "stall.log").read_text()
                    found = {}
                    for pid in children(supervisor.pid):
                        try:
                            cmd = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                            found[Path(os.fsdecode(cmd[0])).name] = pid
                        except FileNotFoundError:
                            continue
                    if "electrum-lite" in found and "bitcoind" in found:
                        core_pid, app_pid = found["bitcoind"], found["electrum-lite"]
                        result = subprocess.run([bitcoincli, "-regtest", f"-datadir={core_dir}", f"-rpcport={rpc_port}", "getblockchaininfo"], capture_output=True, text=True)
                        if result.returncode == 0:
                            break
                    assert time.monotonic() < deadline, (root / "stall.log").read_text()
                    time.sleep(.1)
                os.kill(app_pid, signal.SIGSTOP)
                deadline = time.monotonic() + 100
                while running(core_pid) and time.monotonic() < deadline:
                    time.sleep(.1)
                assert not running(core_pid), "Core still running after stalled daemon"
                os.kill(app_pid, signal.SIGCONT)
                app_pid = None
                assert supervisor.wait(timeout=30) != 0
                text = (root / "stall.log").read_text()
                assert "no responde al control de salud" in text, text
                print("PASS managed Core: stalled daemon triggers Core shutdown")
            finally:
                if app_pid is not None:
                    try:
                        os.kill(app_pid, signal.SIGCONT)
                    except ProcessLookupError:
                        pass
                if supervisor.poll() is None:
                    supervisor.terminate()
                    supervisor.wait(timeout=150)


if __name__ == "__main__":
    main()

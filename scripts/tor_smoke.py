#!/usr/bin/env python3
"""Exercise the actual private Tor process/SOCKS path, offline by default.

Requires a real tor executable; the test wrapper disables Tor's public network.
This tests lifecycle/fail-closed routing, not public anonymity or relay throughput.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

from managed_prune_smoke import RPC, port, query, wait


def control(root, command):
    address = (root / "control-port").read_text().strip().removeprefix("PORT=")
    host, number = address.rsplit(":", 1)
    with socket.create_connection((host, int(number)), timeout=10) as s:
        s.settimeout(10)
        f = s.makefile("rb")
        s.sendall(("AUTHENTICATE " + (root / "control-cookie").read_bytes().hex() + "\r\n").encode())
        assert f.readline().startswith(b"250 ")
        s.sendall((command + "\r\n").encode())
        lines = []
        while True:
            line = f.readline().decode().strip()
            lines.append(line)
            if len(line) < 4 or line[3] == " ":
                break
        return lines


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--bitcoind", default="bitcoind")
    p.add_argument("--electrum-lite", default="target/release/electrum-lite")
    p.add_argument("--tor-bin", default="tor")
    p.add_argument("--public-bootstrap", action="store_true", help="Explicitly permit public Tor bootstrap; no wallet data leaves")
    a = p.parse_args()
    import shutil
    tor = shutil.which(a.tor_bin)
    if not tor:
        raise SystemExit("Install tor or pass --tor-bin /path/to/tor")
    binary = str(Path(a.electrum_lite).resolve())
    base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="el-tor-smoke-", dir=base) as tmp:
        root = Path(tmp)
        rpcport, p2p, proxyport, listen, socks = [port() for _ in range(5)]
        core = root / "core"
        core.mkdir()
        wrapper = root / "wrapper"
        wrapper.mkdir()
        (wrapper / "tor").write_text("#!/bin/sh\nexec " + shlex.quote(tor) + ' "$@" --DisableNetwork ' + ("0" if a.public_bootstrap else "1") + "\n")
        (wrapper / "tor").chmod(0o700)
        env = dict(os.environ, PATH=str(wrapper) + os.pathsep + os.environ["PATH"])
        children, logs = [], []
        def launch(command, name, environment=None):
            f = open(root / (name + ".log"), "w+")
            logs.append(f)
            child = subprocess.Popen(command, stdout=f, stderr=f, env=environment)
            children.append(child)
            return child
        rpc = RPC(core / "regtest/.cookie", rpcport)
        hide = False
        class Proxy(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                if hide and json.loads(body)["method"] == "getblock":
                    result = json.dumps({"id": 1, "result": None, "error": {"code": -1, "message": "simulated unavailable block"}}).encode()
                else:
                    request = urllib.request.Request(f"http://127.0.0.1:{rpcport}", body, {"Authorization": self.headers["Authorization"]})
                    try:
                        response = urllib.request.urlopen(request, timeout=30)
                    except urllib.error.HTTPError as e:
                        response = e
                    result = response.read()
                self.send_response(200)
                self.send_header("Content-Length", str(len(result)))
                self.end_headers()
                self.wfile.write(result)
        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", proxyport), Proxy)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        sentinel = socket.socket()
        sentinel.bind(("127.0.0.1", 0))
        sentinel.listen()
        sentinel.setblocking(False)
        def no_direct():
            try:
                s, _ = sentinel.accept()
            except BlockingIOError:
                return
            s.close()
            raise AssertionError("unexpected direct connection in Tor mode")
        command = [binary, "--network", "regtest", "--data-dir", str(root / "lite"), "--core-cookie", str(core / "regtest/.cookie"), "--core-rpc", f"http://127.0.0.1:{proxyport}", "--listen", f"127.0.0.1:{listen}"]
        try:
            launch([a.bitcoind, "-regtest", f"-datadir={core}", f"-rpcport={rpcport}", f"-port={p2p}", f"-bind=127.0.0.1:{p2p}", "-server=1", "-listen=1", "-connect=0", "-dnsseed=0", "-discover=0", "-listenonion=0", "-fallbackfee=0.00001", "-dbcache=64"], "core")
            wait(lambda: rpc("getblockcount") == 0, "Core startup")
            rpc("createwallet", "tor-test")
            address = rpc("getnewaddress")
            rpc("generatetoaddress", 101, address)
            txid = rpc("sendtoaddress", rpc("getnewaddress"), 1.0)
            blockhash=rpc("generatetoaddress", 1, address)[0]
            raw = rpc("getrawtransaction", txid,False,blockhash)
            app = launch(command + ["--tor", "--tor-socks", f"127.0.0.1:{socks}", "--block-peer", f"127.0.0.1:{sentinel.getsockname()[1]}", "serve"], "app-tor", env)
            wait(lambda: query(listen, "server.health")["ready"], "Tor app startup",guard=app)
            assert query(listen, "server.health")["transport"] == "socks5"
            torroot = root / "lite/regtest/tor"
            wait(lambda: (torroot / "control-cookie").exists() and (torroot / "control-port").exists(), "private Tor control")
            info = control(torroot, "GETINFO version")
            assert any("version=" in line for line in info)
            print("PASS: actual private Tor process started and identified over cookie-authenticated control", flush=True)
            if a.public_bootstrap:
                def bootstrapped():
                    return any("PROGRESS=100" in line for line in control(torroot, "GETINFO status/bootstrap-phase"))
                wait(bootstrapped, "public Tor bootstrap", timeout=180)
                print("PASS: actual Tor public bootstrap reached 100% (no wallet queries sent)", flush=True)
            hide = True
            try:
                query(listen, "blockchain.transaction.get", txid)
            except RuntimeError:
                pass
            else:
                raise AssertionError("unavailable Tor block should fail")
            no_direct()
            assert not list((root / "lite/regtest/cache").glob("*.blk"))
            control(torroot, "SIGNAL HALT")
            time.sleep(0.5)
            try:
                query(listen, "blockchain.transaction.get", txid)
            except RuntimeError:
                pass
            else:
                raise AssertionError("dead Tor must not fall back")
            no_direct()
            app.terminate()
            assert app.wait(timeout=45) == 0
            print("PASS: rejected Tor request and stopped Tor both fail closed; sentinel observed zero direct connections", flush=True)
            # Tor is optional: the same local index works with explicit direct P2P.
            app = launch(command + ["--block-peer", f"127.0.0.1:{p2p}", "serve"], "app-direct")
            wait(lambda: query(listen, "server.health")["ready"], "direct app startup")
            assert query(listen, "server.health")["transport"] == "direct"
            assert query(listen, "blockchain.transaction.get", txid) == raw
            print("PASS: explicit direct mode retrieves byte-exact witness transaction from local P2P; Tor remains optional", flush=True)
        except Exception:
            for f in logs:
                f.flush()
                f.seek(0)
                print(f"--- {f.name} ---\n{f.read()[-5000:]}")
            raise
        finally:
            proxy.shutdown()
            sentinel.close()
            for child in reversed(children):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=45)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            for f in logs:
                f.close()


if __name__ == "__main__":
    main()

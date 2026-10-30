#!/usr/bin/env python3
"""Opt-in, bounded public Tor/P2P retrieval of MAINNET GENESIS ONLY.

No wallet, no Core public sync, no pruning, no Electrum/HTTP public API.
Tests availability/routing, never anonymity. Public peers can refuse service.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import tempfile
import threading
import urllib.error
import urllib.request

from managed_prune_smoke import RPC, port, query, wait
from tor_smoke import control

p = argparse.ArgumentParser()
p.add_argument("--bitcoind", default="bitcoind")
p.add_argument("--electrum-lite", default="target/release/electrum-lite")
p.add_argument("--tor-bin", default="tor")
p.add_argument("--allow-public", action="store_true")
p.add_argument("--peer", action="append", help="Explicit archival mainnet P2P host:port (through Tor)")
a = p.parse_args()
if not a.allow_public:
    raise SystemExit("Requires --allow-public: downloads genesis through Tor from public Bitcoin peers")
tor = shutil.which(a.tor_bin)
if not tor:
    raise SystemExit("Tor executable required")
binary = str(Path(a.electrum_lite).resolve())
peers = a.peer or ["seed.bitcoin.sipa.be:8333", "seed.bitcoin.jonasschnelli.ch:8333", "seed.bitcoin.sprovoost.nl:8333"]
base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
with tempfile.TemporaryDirectory(prefix="el-tor-genesis-", dir=base) as tmp:
    root = Path(tmp)
    core = root / "core"
    core.mkdir()
    wrapper = root / "wrapper"
    wrapper.mkdir()
    (wrapper / "tor").write_text("#!/bin/sh\nexec " + shlex.quote(tor) + ' "$@"\n')
    (wrapper / "tor").chmod(0o700)
    rpcport, proxyport, listen, socks = [port() for _ in range(4)]
    children, files = [], []
    hide = False
    class Proxy(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass
        def do_POST(self):
            data = self.rfile.read(int(self.headers["Content-Length"]))
            if hide and json.loads(data)["method"] == "getblock":
                body = json.dumps({"id": 1, "result": None, "error": {"code": -1, "message": "simulated unavailable genesis"}}).encode()
            else:
                request = urllib.request.Request(f"http://127.0.0.1:{rpcport}", data, {"Authorization": self.headers["Authorization"]})
                try:
                    response = urllib.request.urlopen(request, timeout=30)
                except urllib.error.HTTPError as e:
                    response = e
                body = response.read()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", proxyport), Proxy)
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    def launch(command, name, env=None):
        f = open(root / name, "w+")
        files.append(f)
        child = subprocess.Popen(command, stdout=f, stderr=f, env=env)
        children.append(child)
        return child
    try:
        launch([a.bitcoind, f"-datadir={core}", f"-rpcport={rpcport}", "-server=1", "-listen=0", "-connect=0", "-dnsseed=0", "-discover=0", "-listenonion=0", "-disablewallet", "-dbcache=64"], "core.log")
        rpc = RPC(core / ".cookie", rpcport)
        wait(lambda: rpc("getblockcount") == 0, "offline genesis-only Core")
        genesis = rpc("getblockhash", 0)
        txid = rpc("getblock", genesis, 1)["tx"][0]
        raw_block=rpc("getblock",genesis,0)
        assert raw_block[160:162]=="01"
        original = raw_block[162:]
        command = [binary, "--network", "mainnet", "--data-dir", str(root / "lite"), "--core-cookie", str(core / ".cookie"), "--core-rpc", f"http://127.0.0.1:{proxyport}", "--listen", f"127.0.0.1:{listen}", "--tor", "--tor-socks", f"127.0.0.1:{socks}"]
        for peer in peers:
            command += ["--block-peer", peer]
        launch(command + ["serve"], "app.log", dict(os.environ, PATH=str(wrapper) + os.pathsep + os.environ["PATH"]))
        wait(lambda: query(listen, "server.health")["ready"], "genesis index")
        torroot = root / "lite/mainnet/tor"
        wait(lambda: any("PROGRESS=100" in line for line in control(torroot, "GETINFO status/bootstrap-phase")), "Tor public bootstrap", timeout=180)
        hide = True
        with socket.create_connection(("127.0.0.1", listen), timeout=10) as s:
            s.settimeout(300)
            s.sendall(json.dumps({"id": 1, "method": "blockchain.transaction.get", "params": [txid]}).encode() + b"\n")
            result = json.loads(s.makefile("rb").readline(10000))
        assert result.get("result") == original, result.get("error")
        assert (root / "lite/mainnet/cache" / (genesis + ".blk")).is_file()
        assert rpc("getblockcount") == 0
        print("PASS: real Tor public bootstrap and Bitcoin P2P witness-block retrieval; byte-exact mainnet genesis transaction verified")
        print("PASS: Core stayed offline at height 0; only genesis requested; no wallet or public API used; not full mainnet sync")
    except Exception:
        for f in files:
            f.flush()
            f.seek(0)
            print(f"--- {f.name} ---\n{f.read()[-5000:]}")
        raise
    finally:
        proxy.shutdown()
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        for f in files:
            f.close()

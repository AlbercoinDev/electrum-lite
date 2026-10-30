#!/usr/bin/env python3
"""Compare sync on an isolated regtest and inject a block-fetch failure."""
import argparse
import http.server
import json
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
from pathlib import Path


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bitcoind", required=True)
    parser.add_argument("--bitcoin-cli", required=True)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--transactions", type=int, default=0, help="additional wallet transactions after coinbase maturity")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="prefetch-smoke-", dir="/tmp/opencode") as tmp:
        root = Path(tmp)
        core = root / "core"
        core.mkdir()
        rpc, p2p, proxyport = port(), port(), port()
        node = subprocess.Popen([args.bitcoind, "-regtest", f"-datadir={core}", f"-rpcport={rpc}", f"-port={p2p}", "-server=1", "-listen=0", "-connect=0", "-dnsseed=0", "-fallbackfee=0.00001"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        failure = None
        wrong_hash = None
        heights = []
        block_requests = []

        class Proxy(http.server.BaseHTTPRequestHandler):
            def log_message(self, *unused):
                pass

            def do_POST(self):
                data = self.rfile.read(int(self.headers["Content-Length"]))
                message = json.loads(data)
                if isinstance(message, list):
                    heights.extend(item["params"][0] for item in message if item["method"] == "getblockhash")
                    request = urllib.request.Request(f"http://127.0.0.1:{rpc}", data, {"Authorization": self.headers["Authorization"]})
                    with urllib.request.urlopen(request, timeout=30) as response:
                        replies = json.load(response)
                    for item in message:
                        if item["method"] == "getblockhash" and item["params"] == [20] and failure:
                            reply = next(r for r in replies if r["id"] == item["id"])
                            if failure == "rpc":
                                reply.update(result=None, error={"code": -1, "message": "injected"})
                            elif failure == "branch":
                                reply.update(result=wrong_hash, error=None)
                    result = json.dumps(replies).encode()
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(result)))
                    self.end_headers()
                    self.wfile.write(result)
                    return
                if message["method"] == "getblockhash":
                    heights.append(message["params"][0])
                if message["method"] == "getblock":
                    block_requests.append(message["params"][0])
                if failure == "rpc" and message["method"] == "getblockhash" and message["params"] == [20]:
                    result = json.dumps({"id": 1, "result": None, "error": {"code": -1, "message": "injected"}}).encode()
                elif failure == "branch" and message["method"] == "getblockhash" and message["params"] == [20]:
                    result = json.dumps({"id": 1, "result": wrong_hash, "error": None}).encode()
                else:
                    request = urllib.request.Request(f"http://127.0.0.1:{rpc}", data, {"Authorization": self.headers["Authorization"]})
                    with urllib.request.urlopen(request, timeout=30) as response:
                        result = response.read()
                self.send_response(200)
                self.send_header("Content-Length", str(len(result)))
                self.end_headers()
                self.wfile.write(result)

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", proxyport), Proxy)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()

        def cli(*command):
            return subprocess.run([args.bitcoin_cli, "-regtest", f"-datadir={core}", f"-rpcport={rpc}", *map(str, command)], capture_output=True, text=True, timeout=30, check=True).stdout.strip()

        def run(binary, name, command="sync", rpc_port=rpc):
            return subprocess.run([str(Path(binary).resolve()), "--network", "regtest", "--data-dir", str(root / name), "--core-cookie", str(core / "regtest/.cookie"), "--core-rpc", f"http://127.0.0.1:{rpc_port}", command], capture_output=True, text=True, timeout=90)

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
            cli("createwallet", "bench")
            miner = cli("getnewaddress")
            cli("generatetoaddress", 101, miner)
            mined = 101
            for i in range(args.transactions):
                cli("sendtoaddress", miner, "0.01")
                if (i + 1) % 10 == 0:
                    cli("generatetoaddress", 1, miner)
                    mined += 1
            if mined >= 300:
                raise ValueError("too many transactions for the 300-block test")
            cli("generatetoaddress", 300 - mined, miner)
            for name, binary in [("old1", args.baseline), ("new1", args.candidate), ("new2", args.candidate), ("old2", args.baseline)]:
                start = time.monotonic()
                result = run(binary, name)
                elapsed = time.monotonic() - start
                assert result.returncode == 0, result.stderr
                for line in result.stderr.splitlines():
                    if line.startswith("sync_metrics"):
                        print(f"{name}: {line}", flush=True)
                health = run(binary, name, "health")
                assert health.returncode == 0 and json.loads(health.stdout)["indexed_height"] == 300
                print(f"{name}: 301 blocks in {elapsed:.3f}s", flush=True)
            failure = "rpc"
            result = run(args.candidate, "fault", rpc_port=proxyport)
            assert result.returncode != 0 and "getblockhash" in result.stderr, result.stderr
            assert max(heights) <= 31, heights
            failure = None
            resumed = run(args.candidate, "fault", rpc_port=proxyport)
            assert resumed.returncode == 0, resumed.stderr
            health = run(args.candidate, "fault", "health")
            assert json.loads(health.stdout)["indexed_height"] == 300
            print("PASS: hash-batch failure stops sync; restart reaches height 300", flush=True)
            heights.clear()
            block_requests.clear()
            wrong_hash = cli("getblockhash", 0)
            failure = "branch"
            result = run(args.candidate, "branch-fault", rpc_port=proxyport)
            assert result.returncode != 0 and "chain discontinuity" in result.stderr, result.stderr
            assert max(heights) <= 31, heights
            assert len(block_requests) <= 22, len(block_requests)
            failure = None
            resumed = run(args.candidate, "branch-fault", rpc_port=proxyport)
            assert resumed.returncode == 0, resumed.stderr
            health = run(args.candidate, "branch-fault", "health")
            assert json.loads(health.stdout)["indexed_height"] == 300
            print("PASS: wrong-branch block rejected; hash batch bounded; restart succeeds", flush=True)
        finally:
            proxy.shutdown()
            if node.poll() is None:
                node.terminate()
                node.wait(timeout=30)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Create a NEW offline regtest for manual wallet testing. Does not delete anything."""
import argparse
import json
from pathlib import Path
import shlex
import subprocess

p = argparse.ArgumentParser()
p.add_argument("--root", required=True, type=Path)
p.add_argument("--bitcoind", default="bitcoind")
p.add_argument("--bitcoin-cli", default="bitcoin-cli")
p.add_argument("--rpcport", type=int, default=19443)
p.add_argument("--port", type=int, default=19444)
a = p.parse_args()
root = a.root.expanduser().resolve()
if root.exists():
    raise SystemExit("Refusing existing root. Choose a NEW directory; nothing deleted.")
root.mkdir(parents=True)
core = root / "core"
core.mkdir()
subprocess.run([a.bitcoind, "-regtest", f"-datadir={core}", f"-rpcport={a.rpcport}", f"-port={a.port}", "-daemonwait", "-server=1", "-listen=0", "-connect=0", "-dnsseed=0", "-discover=0", "-prune=1", "-fallbackfee=0.00001"], check=True)
cli = [a.bitcoin_cli, "-regtest", f"-datadir={core}", f"-rpcport={a.rpcport}"]
def call(method, *args):
    out = subprocess.check_output(cli + [method, *map(str, args)], text=True)
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return out.strip()
call("createwallet", "miner")
miner = call("getnewaddress")
call("generatetoaddress", 101, miner)
(root / "README.txt").write_text("Disposable offline regtest. Stop using the command printed below. No automatic deletion.\n")
print("\nStart Electrum Lite in another terminal:\n")
print(shlex.join(["electrum-lite", "--network", "regtest", "--data-dir", str(root / "lite"), "--core-cookie", str(core / "regtest" / ".cookie"), "--core-rpc", f"http://127.0.0.1:{a.rpcport}", "serve"]))
print("\nConnect the Electrum wallet:\nelectrum --regtest --oneserver --server 127.0.0.1:50001:t")
print("\nFund an address copied from the wallet:\n" + shlex.join(cli + ["sendtoaddress", "WALLET_ADDRESS", "1.0"]))
print("\nConfirm transactions:\n" + shlex.join(cli + ["generatetoaddress", "1", miner]))
print("\nStop Electrum Lite with Ctrl+C, then stop this Core:\n" + shlex.join(cli + ["stop"]))

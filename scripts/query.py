#!/usr/bin/env python3
"""One Electrum TCP request, for local diagnostics without a wallet."""
import argparse
import json
import socket

p = argparse.ArgumentParser()
p.add_argument("--host", default="127.0.0.1")
p.add_argument("--port", default=50001, type=int)
p.add_argument("method", nargs="?", default="server.health")
p.add_argument("params", nargs="?", default="[]", help="JSON array")
a = p.parse_args()
params = json.loads(a.params)
if not isinstance(params, list):
    raise SystemExit("params must be a JSON array")
with socket.create_connection((a.host, a.port), timeout=10) as s:
    s.settimeout(120)
    s.sendall(json.dumps({"id": 1, "method": a.method, "params": params}).encode() + b"\n")
    response = json.loads(s.makefile("rb").readline(16_200_001))
    print(json.dumps(response, indent=2))
    if response.get("error") is not None:
        raise SystemExit(1)

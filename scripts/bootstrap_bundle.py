#!/usr/bin/env python3
"""Download a consistent installer source snapshot over GitHub HTTPS."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.request


def fetch(url, maximum):
    request = urllib.request.Request(url, headers={"User-Agent": "electrum-lite-bootstrap"})
    with urllib.request.urlopen(request, timeout=60) as response:
        if not response.url.startswith("https://"):
            raise RuntimeError("Non-HTTPS redirect refused")
        data = response.read(maximum + 1)
    if len(data) > maximum:
        raise RuntimeError("Bootstrap download too large")
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.home() / "electrum-lite")
    args = parser.parse_args()
    if sys.version_info < (3, 11) or os.geteuid() == 0:
        raise RuntimeError("Python 3.11+ required; run without sudo")
    root = args.root.expanduser().absolute()
    if root.is_symlink():
        raise RuntimeError("Symlink installation root refused")
    root.mkdir(exist_ok=True, parents=True, mode=0o700)
    temporary = root / "tmp"
    if temporary.is_symlink():
        raise RuntimeError("Symlink temporary directory refused")
    temporary.mkdir(exist_ok=True, mode=0o700)
    repository = "AlbercoinDev/electrum-lite"
    commit = json.loads(fetch(f"https://api.github.com/repos/{repository}/commits/main", 1024 * 1024))["sha"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise RuntimeError("Invalid installer source commit")
    with tempfile.TemporaryDirectory(dir=temporary, prefix="bootstrap-") as directory:
        for name in ("install.py", "core_verify.py", "bundle_policy.py", "bundle_launcher.py", "install_bundle.py"):
            content = fetch(f"https://raw.githubusercontent.com/{repository}/{commit}/scripts/{name}", 1024 * 1024)
            (Path(directory) / name).write_bytes(content)
        print(f"Installer source: {commit}; trust: GitHub HTTPS and pinned Core signer", flush=True)
        subprocess.run([sys.executable, str(Path(directory) / "install_bundle.py"), "--root", str(root)], check=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(f"Bootstrap refused: {error}")

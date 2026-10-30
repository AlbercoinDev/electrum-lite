#!/usr/bin/env python3
"""Launch the installed pair through Lite's dedicated-Core supervisor (Linux)."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re

from bundle_policy import bundle_lock


def real_directory(root, relative):
    path = root
    for component in Path(relative).parts:
        path = path / component
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise RuntimeError(f"Managed directory is not a real directory: {path}")
        path.mkdir(exist_ok=True, mode=0o700)
    return path


def launch(root, network, offline=False):
    if network not in ("signet", "testnet4", "regtest"):
        raise RuntimeError("Managed mainnet remains gated pending safety validation")
    root = Path(root).absolute()
    with bundle_lock(root, updating=False):
        active = root / "active.json"
        if active.is_symlink() or not active.is_file() or active.stat().st_size > 8192:
            raise RuntimeError("Missing or invalid installed pair manifest")
        manifest = json.loads(active.read_text())
        if manifest.get("format") != 1:
            raise RuntimeError("Unsupported bundle format")
        binaries = []
        for kind, filename in (("lite", "electrum-lite"), ("core", "bitcoind")):
            relative = manifest.get(kind, "")
            family = "lite" if kind == "lite" else "bitcoin-core"
            if not isinstance(relative, str) or not re.fullmatch(f"versions/{family}/[0-9a-f]{{64}}", relative):
                raise RuntimeError("Invalid installed executable path")
            target = root / relative / filename
            if (target.resolve() != target or not target.is_file()
                    or not os.access(target, os.X_OK)):
                raise RuntimeError("Missing executable or symlink in installed pair")
            binaries.append(target)
        # New engine, new generation: never reuse/adapt an older index or an
        # already-pruned Core that cannot supply the new index's history.
        network_root = real_directory(root, f"data/{network}/lsm-v3")
        # Supervisor must create a NEW Core root itself and write its ownership
        # marker. Precreating an empty directory would correctly be rejected.
        core_root = network_root / "core"
        if core_root.is_symlink() or (core_root.exists() and not core_root.is_dir()):
            raise RuntimeError("Core root must be a real managed directory")
        lite_root = network_root / "lite"
        if lite_root.is_symlink() or (lite_root.exists() and not lite_root.is_dir()):
            raise RuntimeError("Lite root must be a real managed directory")
        backup = real_directory(root, f"backups/{network}")
        # Separate per-network ownership excludes duplicate launches, while the
        # shared bundle lock permits independent networks and excludes updates.
        descriptor = os.open(network_root / "runtime.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        try:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise RuntimeError(f"Lite is already running for {network}") from error
            rpc, electrum = {"signet": (39332, 60601), "testnet4": (49332, 60602),
                             "regtest": (19443, 60603)}[network]
            command = [str(binaries[0]), "--network", network, "--data-dir", str(lite_root),
                       "managed-serve", "--lsm", "--bitcoind", str(binaries[1]),
                       "--core-dir", str(core_root), "--rpc-port", str(rpc),
                       "--listen", f"127.0.0.1:{electrum}", "--confirm-dedicated",
                       "--auto-prune", "--prune-backup-root", str(backup)]
            if offline:
                command.append("--offline")
            # Stay the same PID: signals go directly to the existing supervisor.
            # Both locks survive exec and remain held until supervisor exit.
            os.set_inheritable(descriptor, True)
            os.execv(command[0], command)
        finally:
            os.close(descriptor)


def main(root=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--network", required=True, choices=("signet", "testnet4", "regtest"))
    parser.add_argument("--offline", action="store_true", help="Disable P2P for isolated tests")
    arguments = parser.parse_args()
    launch(root or Path(__file__).resolve().parent.parent, arguments.network, arguments.offline)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, ValueError) as error:
        raise SystemExit(f"Lite startup refused: {error}")

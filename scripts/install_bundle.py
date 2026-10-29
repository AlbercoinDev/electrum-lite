#!/usr/bin/env python3
"""Transactional bundle preparation; not yet the public bootstrap entry point.

Callers must hold the installation lock and exclude running managed services.
Activation changes one manifest, never the data or a personal Core installation.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import re
import tarfile
import tempfile
import argparse
import platform
import sys
from contextlib import redirect_stdout

from core_verify import verify_checksums, verify_package
from install import API, MAX_ARCHIVE, download, install, select_assets
from bundle_policy import (CORE_KEY_URL, CORE_RELEASE_API, VALIDATED_CORE_VERSIONS, VALIDATED_PAIRS,
                           bundle_lock, official_core_release)


ENTRYPOINT = b'''#!/usr/bin/env python3
import fcntl, json, os, re, sys
from pathlib import Path
sys.dont_write_bytecode = True
root = Path(__file__).absolute().parent
fd = os.open(root / "bundle.lock", os.O_RDWR | os.O_NOFOLLOW)
try:
    fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
    os.set_inheritable(fd, True)
    manifest = json.loads((root / "active.json").read_text())
    runtime = manifest.get("runtime", "")
    if not re.fullmatch(r"versions/runtime/[0-9a-f]{64}", runtime):
        raise RuntimeError("Invalid runtime manifest")
    path = root / runtime
    if path.resolve() != path:
        raise RuntimeError("Runtime symlinks refused")
    sys.path.insert(0, str(path))
    from bundle_launcher import main
    main(root)
except (OSError, RuntimeError, ValueError) as error:
    sys.exit(f"Lite startup refused: {error}")
finally:
    os.close(fd)
'''


def install_latest(root):
    """Select official latest releases; never silently fall back to older Core."""
    with bundle_lock(root, updating=True):
        version, base = official_core_release(json.loads(download(CORE_RELEASE_API, 4 * 1024 * 1024)))
        if version not in VALIDATED_CORE_VERSIONS:
            raise RuntimeError(f"Core {version} is the latest official stable release, but this Lite "
                               "installer has no validated integration for it; installation unchanged")
        release = json.loads(download(API, 4 * 1024 * 1024))
        package, checksum = select_assets(release)
        lite_payload = download(package["browser_download_url"], MAX_ARCHIVE)
        if hashlib.sha256(lite_payload).hexdigest() not in VALIDATED_PAIRS[version]:
            raise RuntimeError("Latest Lite artifact has not been integration-tested with latest Core")
        return _install_bundle(
            root, package, download(checksum["browser_download_url"], 4096),
            lite_payload, version,
            download(base + f"bitcoin-{version}-x86_64-linux-gnu.tar.gz", MAX_ARCHIVE),
            download(base + "SHA256SUMS", 1024 * 1024),
            download(base + "SHA256SUMS.asc", 1024 * 1024),
            download(CORE_KEY_URL, 1024 * 1024), VALIDATED_CORE_VERSIONS)


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def install_bundle(root, *args, **kwargs):
    with bundle_lock(root, updating=True):
        return _install_bundle(root, *args, **kwargs)


def _install_bundle(root, package, checksum, lite_payload, core_version,
                   core_payload, sums, signatures, key, compatible_versions):
    """Verify, stage, then atomically activate an explicitly tested pair.

    Compatibility must come from release policy, never downloaded Core metadata.
    Old manifests and immutable version directories remain available, but are
    not automatically activated as a downgrade over potentially newer data.
    """
    if (not re.fullmatch(r"[0-9]+\.[0-9]+(?:\.[0-9]+)?", core_version)
            or core_version not in compatible_versions):
        raise RuntimeError("Latest official Core has no validated compatibility policy")
    root = Path(root).absolute()
    if root.is_symlink():
        raise RuntimeError("Installation root must not be a symlink")
    root.mkdir(parents=True, exist_ok=True)
    for name in ("tmp", "versions", "versions/lite", "versions/bitcoin-core", "versions/runtime",
                 "config", "data", "backups", "logs"):
        path = root / name
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise RuntimeError("Unknown installation layout")
        path.mkdir(exist_ok=True)
    active = root / "active.json"
    if active.is_symlink() or (active.exists() and not active.is_file()):
        raise RuntimeError("Unknown activation manifest")
    if active.exists():
        existing = json.loads(active.read_text())
        old_version = existing.get("core_version", "")
        if not re.fullmatch(r"[0-9]+\.[0-9]+(?:\.[0-9]+)?", old_version):
            raise RuntimeError("Invalid installed Core version")
        normalized = lambda value: tuple((list(map(int, value.split("."))) + [0, 0])[:3])
        if normalized(core_version) < normalized(old_version):
            raise RuntimeError("Automatic Core downgrade refused; preserve data and use explicit recovery")
    launcher = root / "electrum-lite"
    if (launcher.is_symlink() or (launcher.exists() and
            (not launcher.is_file() or launcher.read_bytes() != ENTRYPOINT))):
        raise RuntimeError("Refusing to replace unknown launcher")
    with tempfile.TemporaryDirectory(prefix="bundle-", dir=root / "tmp") as temporary:
        staging = Path(temporary)
        authenticated = verify_checksums(sums, signatures, key, staging)
        filename = f"bitcoin-{core_version}-x86_64-linux-gnu.tar.gz"
        verify_package(authenticated, filename, core_payload)
        # Reuse the restricted Lite extractor and executable version check.
        with redirect_stdout(io.StringIO()):
            install(staging / "lite-stage", package, checksum, lite_payload)
        lite = (staging / "lite-stage/share/electrum-lite/current").resolve()
        core = staging / "core-stage"
        core.mkdir()
        with tarfile.open(fileobj=io.BytesIO(core_payload), mode="r:gz") as archive:
            for binary in ("bitcoind", "bitcoin-cli"):
                members = [item for item in archive.getmembers()
                           if item.name == f"bitcoin-{core_version}/bin/{binary}"]
                if (len(members) != 1 or not members[0].isfile()
                        or not 0 < members[0].size <= 128 * 1024 * 1024):
                    raise RuntimeError("Invalid Core executable archive member")
                with archive.extractfile(members[0]) as source:
                    target = core / binary
                    target.write_bytes(source.read(members[0].size + 1))
                    target.chmod(0o755)
        lite_id = hashlib.sha256(lite_payload).hexdigest()
        core_id = hashlib.sha256(core_payload).hexdigest()
        runtime = staging / "runtime"
        runtime.mkdir()
        digest = hashlib.sha256()
        for name in ("bundle_launcher.py", "bundle_policy.py"):
            content = (Path(__file__).parent / name).read_bytes()
            digest.update(name.encode() + b"\0" + content)
            (runtime / name).write_bytes(content)
        manifest = {"format": 1, "lite": f"versions/lite/{lite_id}",
                    "core": f"versions/bitcoin-core/{core_id}", "core_version": core_version,
                    "runtime": f"versions/runtime/{digest.hexdigest()}"}
        for source, relative in ((lite, manifest["lite"]), (core, manifest["core"]),
                                 (runtime, manifest["runtime"])):
            destination = root / relative
            if destination.exists() or destination.is_symlink():
                if (destination.is_symlink() or not destination.is_dir()
                        or sorted(p.name for p in destination.iterdir()) != sorted(p.name for p in source.iterdir())
                        or any(p.is_symlink() or not p.is_file()
                               or p.read_bytes() != (source / p.name).read_bytes()
                               or p.stat().st_mode & 0o777 != (source / p.name).stat().st_mode & 0o777
                               for p in destination.iterdir())):
                    raise RuntimeError("Existing immutable version does not match verified package")
            else:
                for path in source.iterdir():
                    with path.open("rb") as stream:
                        os.fsync(stream.fileno())
                sync_directory(source)
                os.rename(source, destination)
                sync_directory(destination.parent)
        if not launcher.exists():
            prepared_launcher = staging / "electrum-lite"
            prepared_launcher.write_bytes(ENTRYPOINT)
            prepared_launcher.chmod(0o755)
            with prepared_launcher.open("rb") as stream:
                os.fsync(stream.fileno())
            os.rename(prepared_launcher, launcher)
            sync_directory(root)
        pending = staging / "active.json"
        pending.write_text(json.dumps(manifest, sort_keys=True) + "\n")
        with pending.open("rb") as stream:
            os.fsync(stream.fileno())
        os.replace(pending, active)
        sync_directory(root)
    return manifest


def main():
    parser = argparse.ArgumentParser(description="Install/update dedicated Lite and official Bitcoin Core")
    parser.add_argument("--root", type=Path, default=Path.home() / "electrum-lite")
    args = parser.parse_args()
    if sys.version_info < (3, 11) or platform.system() != "Linux" or platform.machine() != "x86_64":
        raise RuntimeError("Linux x86-64 and Python 3.11+ required")
    if os.geteuid() == 0:
        raise RuntimeError("Run as your normal user, without sudo")
    root = args.root.expanduser().absolute()
    install_latest(root)
    print(f"Installed verified experimental pair. Start Signet: {root}/electrum-lite --network signet")
    print("Close with Ctrl+C; managed Core is stopped too. Data and previous versions are retained.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, tarfile.TarError) as error:
        sys.exit(f"Installation refused: {error}")

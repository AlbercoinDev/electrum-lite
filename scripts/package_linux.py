#!/usr/bin/env python3
"""Package an already built Linux executable. No upload or installation side effects."""
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tomllib


def run(*args):
    return subprocess.check_output(args, text=True)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    root = Path(__file__).resolve().parent.parent
    if Path.cwd() != root:
        raise SystemExit("Run from the repository root")
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("Only native Linux x86-64 packaging is supported")
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    commit = run("git", "rev-parse", "HEAD").strip()
    binary = root / "target/release/electrum-lite"
    actual = run(str(binary), "--version").strip()
    if actual != f"electrum-lite {version}":
        raise SystemExit("Binary version does not match Cargo.toml")
    metadata = json.loads(run("cargo", "metadata", "--locked", "--offline", "--format-version", "1"))
    name = f"electrum-lite-{version}-linux-x86_64-{commit[:7]}"
    output = root / "dist"
    output.mkdir(exist_ok=True)
    stage = output / name
    stage.mkdir()  # Never overwrite an earlier package.
    shutil.copy2(binary, stage / "electrum-lite")
    shutil.copy2(root / "packaging/install-linux.sh", stage / "install.sh")
    shutil.copy2(root / "docs/install-linux.md", stage / "INSTALL-LINUX.md")
    shutil.copy2(root / "README.md", stage / "README.md")
    shutil.copytree(root / "docs", stage / "docs")
    notices = stage / "THIRD-PARTY"
    notices.mkdir()
    inventory = []
    # Include notices for all resolved packages, even dev/other-target dependencies.
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        if package["name"] == "electrum-lite":
            continue
        source = Path(package["manifest_path"]).parent
        folder = notices / f'{package["name"]}-{package["version"]}'
        copied = []
        for path in sorted(source.rglob("*")):
            if not path.is_file() or not path.name.lower().startswith(("license", "licence", "copying", "notice", "copyright", "authors")):
                continue
            target = folder / path.relative_to(source)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            copied.append(str(target.relative_to(notices)))
        inventory.append({"name": package["name"], "version": package["version"],
                          "license": package["license"], "repository": package["repository"],
                          "notices": copied})
    (notices / "inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
    (notices / "README.txt").write_text(
        "Notices from Cargo.lock-resolved packages, including development/other-target packages.\n"
        "Not every listed package is linked into the Linux binary. Native bundled notices are included.\n"
        "The project's own license remains undecided; this package grants no new general reuse license.\n")
    (stage / "BUILD-INFO.txt").write_text(
        f"Application: {actual}\nSource commit: {commit}\n"
        f"Working tree modifications at packaging:\n{run('git', 'status', '--short')}\n"
        f"Toolchain: {run('rustc', '--version')}"
        f"Target: x86_64-unknown-linux-gnu (generic baseline; no target-cpu=native)\n"
        f"Minimum referenced GLIBC: 2.34 (inspected for this build)\n"
        f"Dynamic libraries:\n{run('ldd', str(binary))}\n"
        f"Cargo.lock SHA256: {digest(root / 'Cargo.lock')}\n"
        "No signature; local experimental package, not a production certification.\n")
    files = sorted(path for path in stage.rglob("*") if path.is_file())
    (stage / "SHA256SUMS").write_text("".join(f"{digest(path)}  {path.relative_to(stage)}\n" for path in files))
    archive = output / f"{name}.tar.gz"
    with archive.open("xb") as stream:
        with tarfile.open(fileobj=stream, mode="w:gz") as tar:
            tar.add(stage, arcname=name)
    checksum = output / f"{archive.name}.sha256"
    checksum.write_text(f"{digest(archive)}  {archive.name}\n")
    print(archive)
    print(checksum)


if __name__ == "__main__":
    main()

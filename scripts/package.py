#!/usr/bin/env python3
"""Package this exact local build, without publishing or including node data."""
import argparse
import hashlib
from pathlib import Path
import tarfile

p = argparse.ArgumentParser()
p.add_argument("--output", type=Path, default=Path("dist/electrum-lite-thin-managed-experimental-linux-x86_64.tar.gz"))
a = p.parse_args()
base = Path(__file__).resolve().parent.parent
binary = base / "target/release/electrum-lite"
if not binary.is_file():
    raise SystemExit("Build with cargo build --release --locked first")
if a.output.exists():
    raise SystemExit("Refusing to overwrite an existing package")
a.output.parent.mkdir(parents=True, exist_ok=True)
prefix = "electrum-lite-thin-experimental"
with tarfile.open(a.output, "x:gz") as archive:
    archive.add(binary, arcname=f"{prefix}/bin/electrum-lite")
    for name in ["README.md", "AGENTS.md", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "build.rs", "install.sh", ".gitignore"]:
        archive.add(base / name, arcname=f"{prefix}/{name}")
    for directory in ["src", "docs", "scripts"]:
        for source in sorted((base / directory).rglob("*")):
            if source.is_file() and "__pycache__" not in source.parts:
                archive.add(source, arcname=f"{prefix}/{source.relative_to(base)}")
checksum = hashlib.sha256(a.output.read_bytes()).hexdigest()
manifest = a.output.with_suffix(a.output.suffix + ".sha256")
manifest.write_text(f"{checksum}  {a.output.name}\n")
print(f"{checksum}  {a.output}")
print("Local artifact only; not a signed release, not published to GitHub.")

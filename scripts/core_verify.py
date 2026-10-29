#!/usr/bin/env python3
"""Authenticate Core release checksums using an explicitly pinned signer.

This module does not select releases, execute Core, or modify installations.
The key is obtained separately from bitcoin-core/guix.sigs; possession of an
arbitrary key accompanying a release is not sufficient to establish trust.
"""
import hashlib
from pathlib import Path
import re
import subprocess
import tempfile


FANQUAKE_PRIMARY = "E777299FC265DD04793070EB944D35F9AC3DB76A"


def verify_checksums(checksums, signatures, key, temporary_root):
    """Return authenticated checksum bytes, or fail closed.

    A private, ephemeral GPG home prevents user keyrings, auto key retrieval,
    and local trust settings from affecting the result. Unknown additional
    signatures are allowed only when the pinned primary key signed the file.
    """
    if not all(isinstance(value, bytes) and 0 < len(value) <= 1024 * 1024
               for value in (checksums, signatures, key)):
        raise RuntimeError("Invalid or oversized Core signature metadata")
    with tempfile.TemporaryDirectory(prefix="core-verify-", dir=temporary_root) as directory:
        home = Path(directory)
        home.chmod(0o700)
        paths = [home / name for name in ("SHA256SUMS", "SHA256SUMS.asc", "signer.asc")]
        for path, data in zip(paths, (checksums, signatures, key)):
            path.write_bytes(data)
        command = ["gpg", "--no-options", "--homedir", str(home), "--batch",
                   "--no-auto-key-retrieve"]
        shown = subprocess.run(command + ["--with-colons", "--show-keys", str(paths[2])],
                               capture_output=True, text=True, timeout=30, check=True)
        primary = []
        pending = False
        for line in shown.stdout.splitlines():
            fields = line.split(":")
            if fields[0] == "pub":
                pending = True
            elif fields[0] == "fpr" and pending:
                primary.append(fields[9])
                pending = False
        if primary != [FANQUAKE_PRIMARY]:
            raise RuntimeError("Core signer key does not match pinned primary fingerprint")
        subprocess.run(command + ["--import", str(paths[2])], capture_output=True,
                       timeout=30, check=True)
        verified = subprocess.run(command + ["--status-fd", "1", "--verify",
                                             str(paths[1]), str(paths[0])],
                                  capture_output=True, text=True, timeout=30)
        valid = False
        for line in verified.stdout.splitlines():
            fields = line.split()
            if fields[:2] == ["[GNUPG:]", "VALIDSIG"] and len(fields) >= 12:
                valid |= fields[11] == FANQUAKE_PRIMARY
        if not valid:
            raise RuntimeError("No valid Core checksum signature from pinned signer")
    return checksums


def verify_package(checksums, filename, payload):
    """Require exactly one matching SHA256 entry, with an exact filename."""
    if not re.fullmatch(r"bitcoin-[0-9]+\.[0-9]+(?:\.[0-9]+)?-x86_64-linux-gnu\.tar\.gz", filename):
        raise RuntimeError("Unexpected Core package name")
    matches = []
    for line in checksums.decode("ascii").splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1] == filename:
            matches.append(fields[0])
    if (len(matches) != 1 or not re.fullmatch(r"[0-9a-f]{64}", matches[0])
            or hashlib.sha256(payload).hexdigest() != matches[0]):
        raise RuntimeError("Core package checksum mismatch or ambiguous entry")

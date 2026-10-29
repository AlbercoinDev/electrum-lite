#!/usr/bin/env python3
"""Offline installer regression tests; no real release or user paths touched."""
import hashlib
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

from install import install, select_assets


def fixture(commit, bad_link=False):
    identifier = f"electrum-lite-0.0.1-beta.1-linux-x86_64-{commit}"
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as tar:
        for name, data in (("electrum-lite", b'#!/bin/sh\necho "electrum-lite 0.0.1-beta.1"\n'),
                           ("BUILD-INFO.txt", b"Offline fixture\n")):
            member = tarfile.TarInfo(f"{identifier}/{name}")
            member.size = len(data)
            if bad_link and name == "electrum-lite":
                member.type = tarfile.SYMTYPE
                member.linkname = "/etc/passwd"
            tar.addfile(member, io.BytesIO(data))
    payload = output.getvalue()
    name = identifier + ".tar.gz"
    checksum = f"{hashlib.sha256(payload).hexdigest()}  {name}\n".encode()
    return {"name": name}, checksum, payload


class InstallerTests(unittest.TestCase):
    def test_install_update_idempotence_preserves_previous(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root)
            install(prefix, *fixture("aaaaaaa"))
            base = prefix / "share/electrum-lite"
            original = (base / "current").resolve()
            install(prefix, *fixture("aaaaaaa"))
            install(prefix, *fixture("bbbbbbb"))
            self.assertEqual((base / "previous").resolve(), original)
            self.assertTrue((original / "electrum-lite").exists())
            self.assertEqual((prefix / "bin/electrum-lite").resolve(),
                             (base / "current/electrum-lite").resolve())

    def test_bad_checksum_keeps_current(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root)
            install(prefix, *fixture("aaaaaaa"))
            current = (prefix / "share/electrum-lite/current").resolve()
            package, checksum, payload = fixture("bbbbbbb")
            with self.assertRaises(RuntimeError):
                install(prefix, package, checksum, payload + b"corruption")
            self.assertEqual((prefix / "share/electrum-lite/current").resolve(), current)

    def test_archive_link_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(RuntimeError):
                install(Path(root), *fixture("aaaaaaa", bad_link=True))
            self.assertFalse((Path(root) / "bin/electrum-lite").exists())

    def test_existing_unmanaged_binary_preserved(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root)
            (prefix / "bin").mkdir()
            binary = prefix / "bin/electrum-lite"
            binary.write_text("original")
            with self.assertRaises(RuntimeError):
                install(prefix, *fixture("aaaaaaa"))
            self.assertEqual(binary.read_text(), "original")

    def test_select_newest_complete_pair(self):
        assets = []
        for i, commit in enumerate(("aaaaaaa", "bbbbbbb")):
            name = fixture(commit)[0]["name"]
            assets += [{"name": name, "created_at": str(i), "id": i},
                       {"name": name + ".sha256", "id": i}]
        self.assertIn("bbbbbbb", select_assets({"assets": assets})[0]["name"])


if __name__ == "__main__":
    unittest.main()

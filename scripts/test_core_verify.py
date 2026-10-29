#!/usr/bin/env python3
"""Offline tests; never execute downloaded binaries or touch user keyrings."""
import hashlib
from pathlib import Path
import tempfile
import unittest

from core_verify import verify_checksums, verify_package


class CoreVerifyTests(unittest.TestCase):
    def test_exact_package(self):
        name = "bitcoin-32.1-x86_64-linux-gnu.tar.gz"
        checksum = (hashlib.sha256(b"package").hexdigest() + "  " + name + "\n").encode()
        verify_package(checksum, name, b"package")
        for metadata, payload in ((checksum, b"changed"), (checksum * 2, b"package"),
                                  (b"", b"package")):
            with self.assertRaises(RuntimeError):
                verify_package(metadata, name, payload)

    def test_reject_package_paths(self):
        with self.assertRaises(RuntimeError):
            verify_package(b"", "../bitcoin-32.1-x86_64-linux-gnu.tar.gz", b"")

    def test_metadata_limits_and_cleanup(self):
        with tempfile.TemporaryDirectory() as root:
            for key in (b"", b"x" * (1024 * 1024 + 1)):
                with self.assertRaises(RuntimeError):
                    verify_checksums(b"checksums", b"signature", key, root)
                self.assertEqual(list(Path(root).iterdir()), [])


if __name__ == "__main__":
    unittest.main()

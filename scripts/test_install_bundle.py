import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import subprocess
from unittest.mock import patch

from install_bundle import install_bundle, install_latest
from test_install import fixture


class BundleTests(unittest.TestCase):
    def test_automatic_core_downgrade_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = self.bundle(root, "32.2")
            with self.assertRaisesRegex(RuntimeError, "downgrade"):
                self.bundle(root, "32.1")
            self.assertEqual(json.loads((root / "active.json").read_text()), first)

    def test_installed_entrypoint_executes_and_update_is_idempotent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = self.bundle(root)
            result = subprocess.run([str(root / "electrum-lite"), "--network", "signet", "--offline"],
                                    capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("electrum-lite 0.0.1-beta.1", result.stdout)
            self.assertEqual(first, self.bundle(root))
            self.assertFalse((root / first["runtime"] / "__pycache__").exists())

    def test_latest_official_core_never_falls_back(self):
        release = {"tag_name": "v99.0", "draft": False, "prerelease": False,
                   "html_url": "https://github.com/bitcoin/bitcoin/releases/tag/v99.0"}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch("install_bundle.download", return_value=json.dumps(release).encode()) as download:
                with self.assertRaisesRegex(RuntimeError, "99.0"):
                    install_latest(root)
                self.assertEqual(download.call_count, 1)
            self.assertFalse((root / "active.json").exists())

    def test_latest_lite_requires_exact_tested_artifact(self):
        release = {"tag_name": "v32.1", "draft": False, "prerelease": False,
                   "html_url": "https://github.com/bitcoin/bitcoin/releases/tag/v32.1"}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch("install_bundle.download", side_effect=[json.dumps(release).encode(), b"{}", b"untested"]), \
                    patch("install_bundle.select_assets", return_value=({"browser_download_url": "https://example.org/lite"}, {})):
                with self.assertRaisesRegex(RuntimeError, "artifact"):
                    install_latest(root)
            self.assertFalse((root / "active.json").exists())

    def bundle(self, root, version="32.1", bad=False, commit="abcdef1"):
        package, checksum, lite = fixture(commit)
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w:gz") as archive:
            for name in ("bitcoind", "bitcoin-cli"):
                member = tarfile.TarInfo(f"bitcoin-{version}/bin/{name}")
                member.size = 4
                if bad:
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/etc/passwd"
                archive.addfile(member, io.BytesIO(b"core"))
        core = output.getvalue()
        sums = f"{hashlib.sha256(core).hexdigest()}  bitcoin-{version}-x86_64-linux-gnu.tar.gz\n".encode()
        # Signature cryptography is tested independently; only this boundary is mocked.
        with patch("install_bundle.verify_checksums", return_value=sums):
            return install_bundle(root, package, checksum, lite, version, core,
                                  sums, b"signature", b"key", {"32.1", "32.2"})

    def test_install_idempotent_update_preserves_data(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "data").mkdir()
            (root / "data/keep").write_bytes(b"user data")
            first = self.bundle(root)
            self.assertEqual(first, self.bundle(root))
            updated = self.bundle(root, "32.2", commit="abcdef2")
            self.assertNotEqual(first, updated)
            self.assertEqual(json.loads((root / "active.json").read_text()), updated)
            self.assertTrue((root / first["lite"]).is_dir())
            self.assertEqual((root / "data/keep").read_bytes(), b"user data")
            self.assertEqual(list((root / "tmp").iterdir()), [])

    def test_failed_update_keeps_active_pair(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = self.bundle(root)
            with self.assertRaises(RuntimeError):
                self.bundle(root, "32.2", bad=True)
            self.assertEqual(json.loads((root / "active.json").read_text()), first)
            self.assertEqual(list((root / "tmp").iterdir()), [])

    def test_unvalidated_core_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(RuntimeError, "compatibility"):
                self.bundle(Path(temporary), "99.0")

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from bundle_launcher import launch
from bundle_policy import bundle_lock


class LauncherTests(unittest.TestCase):
    def fixture(self, root):
        manifest = {"format": 1, "lite": "versions/lite/" + "a" * 64,
                    "core": "versions/bitcoin-core/" + "b" * 64}
        for kind, filename in (("lite", "electrum-lite"), ("core", "bitcoind")):
            path = root / manifest[kind] / filename
            path.parent.mkdir(parents=True)
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
        (root / "active.json").write_text(json.dumps(manifest))

    def test_supervisor_arguments_locks_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)

            def execute(binary, command):
                self.assertEqual(binary, command[0])
                for flag, expected in (("--network", "signet"),
                                       ("--core-dir", str(root / "data/signet/core")),
                                       ("--data-dir", str(root / "data/signet/lite"))):
                    self.assertEqual(command[command.index(flag) + 1], expected)
                self.assertIn("--auto-prune", command)
                self.assertIn("--offline", command)
                with self.assertRaises(RuntimeError):
                    with bundle_lock(root, updating=True):
                        self.fail("updater entered during runtime")
                with self.assertRaisesRegex(RuntimeError, "already running"):
                    launch(root, "signet")
                raise OSError("simulate exec failure")

            with patch("bundle_launcher.os.execv", side_effect=execute):
                with self.assertRaisesRegex(OSError, "simulate"):
                    launch(root, "signet", offline=True)
            with bundle_lock(root, updating=True):
                pass

    def test_mainnet_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "mainnet"):
                launch(Path(directory), "mainnet")

    def test_manifest_escape_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            manifest = json.loads((root / "active.json").read_text())
            manifest["core"] = "../../other-core"
            (root / "active.json").write_text(json.dumps(manifest))
            with self.assertRaisesRegex(RuntimeError, "path"):
                launch(root, "signet")

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import bootstrap_bundle


class BootstrapTests(unittest.TestCase):
    def test_sources_pinned_to_one_commit_and_cleaned(self):
        commit = "a" * 40
        urls = []

        def fetch(url, maximum):
            urls.append(url)
            return json.dumps({"sha": commit}).encode() if len(urls) == 1 else b"# source\n"

        def run(command, check):
            installer = Path(command[1])
            self.assertTrue(installer.is_file())
            self.assertEqual(len(list(installer.parent.iterdir())), 5)
            self.assertTrue(check)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "electrum-lite"
            with patch("sys.argv", ["bootstrap_bundle.py", "--root", str(root)]), \
                    patch("bootstrap_bundle.os.geteuid", return_value=1000), \
                    patch("bootstrap_bundle.fetch", side_effect=fetch), \
                    patch("bootstrap_bundle.subprocess.run", side_effect=run):
                bootstrap_bundle.main()
            self.assertEqual(len(urls), 6)
            self.assertTrue(all(f"/{commit}/scripts/" in url for url in urls[1:]))
            self.assertEqual(list((root / "tmp").iterdir()), [])

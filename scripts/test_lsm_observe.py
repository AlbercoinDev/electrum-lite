import tempfile
from pathlib import Path
import unittest

from lsm_observe import disk_bytes


class ObservationTests(unittest.TestCase):
    def test_counts_files_without_following_links_and_refuses_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "block").write_bytes(b"12345")
            (root / "alias").symlink_to(root / "block")
            result = disk_bytes(root)
            self.assertEqual(result["logical_bytes"], 5)
            self.assertEqual(result["files"], 1)
            with self.assertRaises(RuntimeError):
                disk_bytes(root, maximum=0)

    def test_missing_optional_directory_is_empty(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(disk_bytes(Path(directory) / "missing")["files"], 0)

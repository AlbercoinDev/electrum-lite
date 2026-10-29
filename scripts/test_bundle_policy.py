from pathlib import Path
import tempfile
import unittest

from bundle_policy import bundle_lock, official_core_release


class PolicyTests(unittest.TestCase):
    def test_official_stable_selection(self):
        release = dict(tag_name="v32.1", draft=False, prerelease=False,
                       html_url="https://github.com/bitcoin/bitcoin/releases/tag/v32.1")
        self.assertEqual(official_core_release(release),
                         ("32.1", "https://bitcoincore.org/bin/bitcoin-core-32.1/"))
        for changes in ({"prerelease": True}, {"tag_name": "v32.1rc1"},
                        {"html_url": "https://example.org"}, {"draft": True}):
            with self.assertRaises(RuntimeError):
                official_core_release(dict(release, **changes))

    def test_runtime_blocks_updater_and_updater_blocks_runtime(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with bundle_lock(root, updating=False):
                with bundle_lock(root, updating=False):
                    with self.assertRaises(RuntimeError):
                        with bundle_lock(root, updating=True):
                            self.fail("updater entered")
            with bundle_lock(root, updating=True):
                with self.assertRaises(RuntimeError):
                    with bundle_lock(root, updating=False):
                        self.fail("runtime entered")
            with bundle_lock(root, updating=True):
                pass

"""Official release selection and cooperative installation/runtime exclusion."""
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import re


CORE_RELEASE_API = "https://api.github.com/repos/bitcoin/bitcoin/releases/latest"
CORE_KEY_URL = "https://raw.githubusercontent.com/bitcoin-core/guix.sigs/main/builder-keys/fanquake.gpg"
# Evidence: docs/lsm-install-status.md. Scope is the exact published artifact,
# not every future binary sharing the unchanged beta version string.
VALIDATED_PAIRS = {
    "32.1": frozenset({"0e17ba331be0cc674bc540ae5cdbc0855b04fbc6b8f459c2286ab91977ee5ade"}),
}
VALIDATED_CORE_VERSIONS = frozenset(VALIDATED_PAIRS)


def official_core_release(release):
    tag = release.get("tag_name", "")
    if (release.get("draft") is not False or release.get("prerelease") is not False
            or not re.fullmatch(r"v[0-9]+\.[0-9]+(?:\.[0-9]+)?", tag)
            or release.get("html_url") != f"https://github.com/bitcoin/bitcoin/releases/tag/{tag}"):
        raise RuntimeError("Invalid official stable Bitcoin Core release metadata")
    version = tag[1:]
    return version, f"https://bitcoincore.org/bin/bitcoin-core-{version}/"


@contextmanager
def bundle_lock(root, updating):
    """Exclusive updater or shared runtime lock, held for the entire operation.

    Never unlink this file: doing so would let callers lock different inodes.
    A busy runtime is refused, not signalled by PID or process name.
    """
    root = Path(root).absolute()
    if root.is_symlink():
        raise RuntimeError("Bundle root must not be a symlink")
    root.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(root / "bundle.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(descriptor, (fcntl.LOCK_EX if updating else fcntl.LOCK_SH) | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError("Bundle is running or an update is in progress; close Lite first") from error
        # Runtime execs the supervisor without releasing installation exclusion.
        if not updating:
            os.set_inheritable(descriptor, True)
        yield
    finally:
        os.close(descriptor)

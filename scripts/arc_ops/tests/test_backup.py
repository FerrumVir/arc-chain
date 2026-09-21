"""Backup/verify/restore on synthetic data directories; no node."""

import fcntl
import os
import sys
import tarfile
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_ops import backup as bk  # noqa: E402


def data_dir():
    d = tempfile.mkdtemp()
    os.makedirs(os.path.join(d, "dag-wal"))
    for rel, content in {"state.wal": b"wal-bytes", "state-snapshot.bin": b"snap",
                         "state-snapshot.manifest": b"manifest",
                         "genesis.network-hash": b"abc123\n",
                         "consensus-signing-record.bin": b"record",
                         "dag-wal/wal-00000000.bin": b"dag"}.items():
        with open(os.path.join(d, rel), "wb") as fh:
            fh.write(content)
    return d


class Backup(unittest.TestCase):
    def test_backup_verify_restore_round_trip(self):
        src = data_dir()
        out = os.path.join(tempfile.mkdtemp(), "b.tar.gz")
        m = bk.backup(src, out)
        self.assertEqual(m["genesis_binding"], "abc123")
        self.assertEqual(len(m["members"]), 6)
        bk.verify(out)
        dest = os.path.join(tempfile.mkdtemp(), "restored")
        bk.restore(out, dest)
        for member in m["members"]:
            with open(os.path.join(src, member["path"]), "rb") as a, \
                    open(os.path.join(dest, member["path"]), "rb") as b:
                self.assertEqual(a.read(), b.read())

    def test_a_running_node_is_refused(self):
        src = data_dir()
        fd = os.open(os.path.join(src, ".arc-node.lock"), os.O_RDWR | os.O_CREAT, 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)  # the "running node"
        try:
            with self.assertRaises(bk.NodeRunning):
                bk.backup(src, os.path.join(tempfile.mkdtemp(), "b.tar.gz"))
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)

    def test_a_tampered_archive_is_refused_and_nothing_is_restored(self):
        src = data_dir()
        out = os.path.join(tempfile.mkdtemp(), "b.tar.gz")
        bk.backup(src, out)
        # Rewrite the archive with one member's bytes changed.
        bad = out + ".bad"
        with tarfile.open(out, "r:gz") as tin, tarfile.open(bad, "w:gz") as tout:
            for member in tin.getmembers():
                data = tin.extractfile(member).read()
                if member.name == "data/state.wal":
                    data = b"tampered!"
                    member.size = len(data)
                import io
                tout.addfile(member, io.BytesIO(data))
        with self.assertRaises(ValueError):
            bk.verify(bad)
        dest = os.path.join(tempfile.mkdtemp(), "restored")
        with self.assertRaises(ValueError):
            bk.restore(bad, dest)
        self.assertFalse(os.path.exists(dest) and os.listdir(dest))

    def test_restore_refuses_a_non_empty_target_and_backup_a_used_name(self):
        src = data_dir()
        out = os.path.join(tempfile.mkdtemp(), "b.tar.gz")
        bk.backup(src, out)
        with self.assertRaises(FileExistsError):
            bk.backup(src, out)
        with self.assertRaises(FileExistsError):
            bk.restore(out, src)


if __name__ == "__main__":
    unittest.main(verbosity=2)

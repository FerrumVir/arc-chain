"""Backup/verify/restore on synthetic data directories; no node."""

import fcntl
import io
import json
import os
import stat
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

    def test_a_private_file_keeps_mode_0600_through_a_restore(self):
        """The node refuses its own store if a private file is not exactly
        0600, so a restore that only round-trips bytes produces a directory no
        node will open. A real drill caught this: the restored lock file came
        back 0644 and the restarted validator exited with
        "private file ... must have mode 0600 (found 0644)"."""
        src = data_dir()
        private = os.path.join(src, ".arc-private-directory-namespace-test.lock")
        with open(private, "w") as fh:
            fh.write("lock")
        os.chmod(private, 0o600)
        world_readable = os.path.join(src, "genesis.network-hash")
        os.chmod(world_readable, 0o644)

        out = os.path.join(tempfile.mkdtemp(), "modes.tar.gz")
        manifest = bk.backup(src, out)
        modes = {m["path"]: m["mode"] for m in manifest["members"]}
        self.assertEqual(modes[".arc-private-directory-namespace-test.lock"], 0o600)
        self.assertEqual(modes["genesis.network-hash"], 0o644)

        dest = os.path.join(tempfile.mkdtemp(), "restored")
        bk.restore(out, dest)
        self.assertEqual(
            stat.S_IMODE(os.stat(os.path.join(dest, private[len(src) + 1:])).st_mode), 0o600)
        self.assertEqual(
            stat.S_IMODE(os.stat(os.path.join(dest, "genesis.network-hash")).st_mode), 0o644)

    def test_a_widened_mode_in_the_manifest_is_refused(self):
        """An archive whose manifest disagrees with its own member mode is not
        restored: that is the only way a private file could be widened."""
        src = data_dir()
        private = os.path.join(src, ".arc-private-namespace.lock")
        with open(private, "w") as fh:
            fh.write("lock")
        os.chmod(private, 0o600)
        out = os.path.join(tempfile.mkdtemp(), "edited.tar.gz")
        bk.backup(src, out)

        edited = os.path.join(tempfile.mkdtemp(), "edited-manifest.tar.gz")
        with tarfile.open(out, "r:gz") as src_tar, tarfile.open(edited, "w:gz") as dst_tar:
            manifest = json.loads(src_tar.extractfile(bk.MANIFEST).read().decode())
            for m in manifest["members"]:
                if m["path"] == ".arc-private-namespace.lock":
                    m["mode"] = 0o644
            for member in src_tar.getmembers():
                if member.name == bk.MANIFEST:
                    continue
                dst_tar.addfile(member, src_tar.extractfile(member))
            blob = json.dumps(manifest, indent=2, sort_keys=True).encode()
            info = tarfile.TarInfo(bk.MANIFEST)
            info.size = len(blob)
            dst_tar.addfile(info, io.BytesIO(blob))

        dest = os.path.join(tempfile.mkdtemp(), "restored")
        with self.assertRaises(ValueError):
            bk.restore(edited, dest)

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

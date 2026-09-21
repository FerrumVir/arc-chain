"""Backup, verify and restore one ARC node data directory (R6).

    python3 -m arc_ops.backup backup  --data-dir DIR --out ARCHIVE.tar.gz [--binary PATH]
    python3 -m arc_ops.backup verify  ARCHIVE.tar.gz
    python3 -m arc_ops.backup restore ARCHIVE.tar.gz --data-dir NEW_DIR

A backup is taken only from a STOPPED node. A running node holds an
exclusive kernel lock on `<data-dir>/.arc-node.lock`; the backup takes that
same lock, non-blocking, and keeps it for the whole copy - so it refuses a
running node, and a node started during the backup refuses to start instead
of writing underneath it. A live copy of a WAL, a snapshot and a signing
record taken at different instants is not a state any node was ever in.

The archive carries MANIFEST.json: every file's path, size and sha256, the
genesis binding, the snapshot identity (height), the size of the state WAL,
and - if given - the binary that last ran this store and its digest.
`verify` re-hashes every member; `restore` verifies first, refuses a
non-empty target, and verifies again after extracting.

What a restore may be opened with is in docs/operations/backup-restore-upgrade.md:
a store written by a newer binary can contain records an older one refuses
(a snapshot v2, a WalOp::Rebase), so rolling back is restoring the pre-upgrade
backup with the pre-upgrade binary, never pointing an old binary at a new store.

Exit: 0 success, 1 refused or failed.
"""

import argparse
import fcntl
import hashlib
import io
import json
import os
import sys
import tarfile
import time
from typing import Any, Dict, List, Optional

MANIFEST = "MANIFEST.json"
FORMAT = "arc.node-backup.v1"


def _sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


class NodeRunning(Exception):
    pass


def hold_node_lock(data_dir: str):
    """Take the node's own data-directory lock, or raise NodeRunning."""
    path = os.path.join(data_dir, ".arc-node.lock")
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        raise NodeRunning(f"{data_dir} is locked by a running node; stop it first")
    return fd


def _files(data_dir: str) -> List[str]:
    out = []
    for root, _dirs, names in os.walk(data_dir):
        for name in names:
            full = os.path.join(root, name)
            rel = os.path.relpath(full, data_dir)
            # Lock files carry no state; a restored node recreates them.
            if name == ".arc-node.lock" or name.endswith(".tmp"):
                continue
            out.append(rel)
    return sorted(out)


def backup(data_dir: str, out: str, binary: Optional[str] = None) -> Dict[str, Any]:
    data_dir = os.path.abspath(data_dir)
    if os.path.exists(out):
        raise FileExistsError(f"{out} exists; every backup gets a fresh name")
    fd = hold_node_lock(data_dir)
    try:
        members = []
        for rel in _files(data_dir):
            full = os.path.join(data_dir, rel)
            members.append({"path": rel, "bytes": os.path.getsize(full), "sha256": _sha256(full)})
        manifest: Dict[str, Any] = {
            "format": FORMAT,
            "taken_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "source_dir": data_dir,
            "members": members,
        }
        binding = os.path.join(data_dir, "genesis.network-hash")
        if os.path.exists(binding):
            manifest["genesis_binding"] = open(binding).read().strip()
        snap = os.path.join(data_dir, "state-snapshot.manifest")
        if os.path.exists(snap):
            manifest["snapshot_manifest_sha256"] = _sha256(snap)
        wal = os.path.join(data_dir, "state.wal")
        if os.path.exists(wal):
            manifest["state_wal_bytes"] = os.path.getsize(wal)
        if binary:
            manifest["binary"] = {"path": os.path.abspath(binary), "sha256": _sha256(binary)}
        tmp = out + ".partial"
        with tarfile.open(tmp, "w:gz") as tar:
            for m in members:
                tar.add(os.path.join(data_dir, m["path"]), arcname=f"data/{m['path']}",
                        recursive=False)
            blob = json.dumps(manifest, indent=2, sort_keys=True).encode()
            info = tarfile.TarInfo(MANIFEST)
            info.size = len(blob)
            info.mtime = int(time.time())
            tar.addfile(info, io.BytesIO(blob))
        with open(tmp, "rb") as fh:
            os.fsync(fh.fileno())
        os.replace(tmp, out)
        return manifest
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def _read_manifest(tar: tarfile.TarFile) -> Dict[str, Any]:
    member = tar.getmember(MANIFEST)
    manifest = json.loads(tar.extractfile(member).read().decode())
    if manifest.get("format") != FORMAT:
        raise ValueError(f"not an ARC node backup (format {manifest.get('format')!r})")
    return manifest


def verify(archive: str) -> Dict[str, Any]:
    with tarfile.open(archive, "r:gz") as tar:
        manifest = _read_manifest(tar)
        names = set(tar.getnames())
        for m in manifest["members"]:
            name = f"data/{m['path']}"
            if name not in names:
                raise ValueError(f"missing member {m['path']}")
            h = hashlib.sha256()
            fh = tar.extractfile(name)
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
            if h.hexdigest() != m["sha256"]:
                raise ValueError(f"member {m['path']} does not match its recorded digest")
        extra = {n for n in names if n.startswith("data/")} - {f"data/{m['path']}" for m in manifest["members"]}
        if extra:
            raise ValueError(f"unlisted members: {sorted(extra)}")
    return manifest


def restore(archive: str, data_dir: str) -> Dict[str, Any]:
    manifest = verify(archive)
    data_dir = os.path.abspath(data_dir)
    if os.path.exists(data_dir) and os.listdir(data_dir):
        raise FileExistsError(f"{data_dir} is not empty; restore only into an empty directory")
    os.makedirs(data_dir, mode=0o700, exist_ok=True)
    with tarfile.open(archive, "r:gz") as tar:
        for m in manifest["members"]:
            src = tar.extractfile(f"data/{m['path']}")
            dest = os.path.join(data_dir, m["path"])
            if os.path.commonpath([data_dir, os.path.abspath(dest)]) != data_dir:
                raise ValueError(f"member escapes the target: {m['path']}")
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as out:
                out.write(src.read())
                out.flush()
                os.fsync(out.fileno())
    for m in manifest["members"]:
        if _sha256(os.path.join(data_dir, m["path"])) != m["sha256"]:
            raise ValueError(f"restored {m['path']} does not match its recorded digest")
    return manifest


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description="ARC node backup/verify/restore")
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("backup")
    b.add_argument("--data-dir", required=True)
    b.add_argument("--out", required=True)
    b.add_argument("--binary")
    v = sub.add_parser("verify")
    v.add_argument("archive")
    r = sub.add_parser("restore")
    r.add_argument("archive")
    r.add_argument("--data-dir", required=True)
    a = p.parse_args(argv)
    try:
        if a.cmd == "backup":
            m = backup(a.data_dir, a.out, a.binary)
            print(f"backup {a.out}: {len(m['members'])} files")
        elif a.cmd == "verify":
            m = verify(a.archive)
            print(f"verified {a.archive}: {len(m['members'])} files")
        else:
            m = restore(a.archive, a.data_dir)
            print(f"restored {len(m['members'])} files into {a.data_dir}")
        return 0
    except (NodeRunning, FileExistsError, ValueError, OSError, tarfile.TarError, KeyError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

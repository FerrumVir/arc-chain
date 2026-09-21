# Backup, restore, upgrade and rollback of an ARC validator (R6)

Status: procedure + tooling for the v0.8 candidate line, 2026-09-21. It is
for isolated/local networks and for preparing the protected production
rollout; it does not authorize touching a live validator.

## What a node's data directory holds

| File | What it is | Loss means |
|---|---|---|
| `state.wal` | every durable state and history record | the chain's history on this node |
| `state-snapshot.bin` + `.manifest` | state + the last 256 heights of history, at a height | a slower restart (full WAL replay); never correctness |
| `consensus-signing-record.bin` | anti-equivocation: finality votes and absence decisions signed, the durable commit cursor | **the key may sign conflicting statements** - never restore an older copy onto a key that has signed since |
| `dag-wal/` | this node's DAG blocks (bounded to the retention window) | nothing essential: the DAG is rebuilt from peers |
| `native-decisions/` | anti-equivocation for native votes on pending requests | **as above** for pending requests; settled ones are pruned |
| `genesis.network-hash` | the chain this directory is bound to | the node refuses to open |
| `known_peers.json` | peers seen | nothing |

## Backup

A backup is taken from a STOPPED node only:

```
python3 -m arc_ops.backup backup --data-dir <DIR> --out <NEW-ARCHIVE>.tar.gz --binary <BINARY>
python3 -m arc_ops.backup verify <ARCHIVE>.tar.gz
```

The tool takes the node's own lock on `<DIR>/.arc-node.lock` for the whole
copy: it refuses a running node, and a node started meanwhile refuses to
start. The archive's MANIFEST.json records every file's sha256, the genesis
binding, the snapshot, the WAL size and the binary that last ran the store.

## Restore

```
python3 -m arc_ops.backup restore <ARCHIVE>.tar.gz --data-dir <EMPTY-DIR>
```

Verified before and after extraction; refused into a non-empty directory.

**Signing-record rule.** Restoring an older `consensus-signing-record.bin` or
`native-decisions/` onto a validator key that has signed anything since the
backup can make that key equivocate. Restore a signing record only for the
same moment as the rest of the store, and only if the key has not run
elsewhere since. When in doubt, keep the newer signing record: an old state
with a newer record is safe (the node refuses to re-sign), the reverse is not.

## Upgrade (rolling, canary first)

1. Build the new binary through `scripts/arc-build-provenance.sh`; run the
   read-only copy it prints, never `target/`.
2. On ONE validator (the canary): stop it, back it up, start the new binary
   on the same directory. It must rejoin within the DAG retention window of
   its peers (default 4,096 rounds - about 11 minutes at 6 rounds/s); a
   longer outage needs a checkpoint rejoin.
3. `python3 -m arc_ops.check --rpc <all validators>` must PASS - agreement at
   a common height, finality lag, settlement reconciliation.
4. Only then the next validator, one at a time, keeping a quorum up.

## Rollback

Rolling back is **restoring the pre-upgrade backup with the pre-upgrade
binary**. Never point an older binary at a store a newer one has written:

| Written by the newer binary | What an older binary does |
|---|---|
| snapshot format v2 (history window) | ignores it and replays the full WAL - slow, correct |
| `WalOp::Rebase` (adopted checkpoint) | cannot decode the WAL and refuses to open - fail closed |
| a signing record with more entries | fine, it only ever reads the record forward |

If the newer binary adopted a checkpoint, the old binary cannot read that
store at all: restore the backup and let the node rejoin by history (or by
checkpoint, after upgrading again).

## Forward recovery

A node whose store is lost or refused: restore the latest backup, start it;
it rejoins by history if its peers still hold its commit cursor round,
otherwise by adopting an authenticated checkpoint from them. A node with no
usable backup can start from an empty directory bound to the same genesis;
it joins from round 0 while peers still hold it, otherwise by checkpoint.

## Verified so far

- `arc_ops.backup`: 4 synthetic tests (round trip, running node refused,
  tampered archive refused with nothing restored, non-empty target and
  reused archive name refused).
- On a real stopped validator directory from a release run (9 files):
  backup, verify and restore byte-identical; a backup of a RUNNING soak
  validator was refused without disturbing it.
- Not yet: opening a restored directory with the node binary and rejoining
  (to be run after the 24-hour soak, when multi-process load is allowed).

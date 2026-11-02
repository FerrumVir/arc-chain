# ENG-15 / ARC-79: state WAL compaction prerequisites and design

Status: **design and executable characterization only; not a compactor and not
v0.8.12 release evidence**. No runtime persistence behavior changes in this PR.
Source audit: `cd234413` (workspace version 0.8.11, release-candidate source).
The deployed validator executable was not inspected. No validator was accessed.

The missing input is an approved offline validator data directory, including its
genesis configuration, recovery package/marker and consensus signing record.
Private signing keys are not needed. The issue has no attachment or fixture
location. The measured mainnet facts in the issue are supplied observations,
not measurements repeated here. Do not infer a post-compaction size or restart
time from a 12.1 MB snapshot alone.

## Why the WAL grows

These locations refer to the audited source above; this PR does not move the
production code cited below.

| Location | Observed behavior |
| --- | --- |
| `crates/arc-state/src/lib.rs:799` | `with_persistence` always uses `WalWriter::new`, the single-file writer. |
| `crates/arc-state/src/wal.rs:871` | That writer opens the existing append file and starts `writer_loop` with rotation disabled. |
| `crates/arc-state/src/wal.rs:929`, `:1263` | Rotation and retirement exist for segmented WALs; they do not retire `state.wal`. |
| `crates/arc-node/src/consensus.rs:4838` | Periodic snapshots call `publish_durable_snapshot`; success logs a manifest, without reclaiming WAL bytes. Default interval is 1,024 blocks (`:917`). |
| `crates/arc-state/src/lib.rs:10357` | Snapshot publication syncs the WAL, captures state, then publishes a payload and manifest. It does not truncate, rotate, or replace the WAL. |
| `crates/arc-state/src/lib.rs:945`, `:1198` | Normal startup validates the full WAL before adopting a snapshot. Validation applies mutations and checks checkpoint roots. The decoded entries remain in a `Vec`; the writer also validates its append handle. |
| `crates/arc-state/src/recovery.rs:3422`, `:3459`, `:3513` | Recovery startup plans the post-ARCCHKPT WAL, verifies it against a staged checkpoint state, then installs and replays it again. It does not use the ordinary snapshot as its replay base. |

This establishes the missing retirement path. It explains why a snapshot does
not bound disk use or eliminate full-log validation. It does not attribute the
reported wall times or memory peaks to individual functions without profiling.

## Why truncating after a snapshot is unsafe

1. **The current snapshot is optional.** `snapshot::load` failures fall back to
   WAL replay (`lib.rs:10317`). Once a prefix is deleted, that fallback cannot
   reconstruct state. It must become a hard startup error for a mandatory base.
2. **Publication is not an atomic pair.** `snapshot.rs:223` individually writes,
   fsyncs and replaces a file; `:247` replaces the fixed payload name before the
   fixed manifest name. A crash between replacements can leave the previous
   manifest naming the new payload's wrong digest. Full WAL replay currently
   makes this recoverable. Neither fixed file may become the sole recovery base.
3. **Sequences and origins are enforced.** Single-file readers start at sequence
   zero (`wal.rs:2130`, `:2636`, writer `:878`). Normal replay also requires the
   configured genesis prefix (`lib.rs:1136`). Retaining a tail fails sequence
   validation; renumbering it passes framing but fails genesis validation.
4. **ARCCHKPT startup has a different origin.** Its planner requires contiguous
   blocks starting immediately after the signed transition (`recovery.rs:2157`).
   An empty WAL is valid and restarts at that transition, even when a newer
   ordinary snapshot exists. The added recovery test reproduces a tip rollback
   after emptying the WAL and restores the original bytes to prove the fixture
   itself is valid. An empty replacement WAL is therefore especially dangerous.
5. **Recent history is not complete state retention.** Snapshot export
   (`lib.rs:9976`) retains a default 256-height window (`snapshot.rs:56`). Startup
   recovers older blocks, receipts, transaction bodies and logs from the prefix
   (`lib.rs:958`). Dropping it would change observable archive behavior. Preserve
   this data in a separate archive; do not silently approve history pruning.
6. **Not every durable field is in `SnapshotPayload`.** Rebase anchor round is
   carried by WAL rebase records (`wal.rs:584`, `lib.rs:973`). Signed retained
   transaction/account indexes are merged by recovery (`recovery.rs:3549`). DAG
   WAL operations are explicitly outside StateDB replay (`lib.rs:1558`). Audit
   and retain their provenance instead of deleting them by block-height tag.
7. **A height alone is not an exact cut.** Auxiliary commits can share a height.
   Preserve a next-sequence boundary S as well as H. Capture must exclude
   concurrent state mutation. `publish_durable_snapshot` currently has no global
   mutation gate around capture plus `wal.sequence()`; its comment that a later
   sequence read can only be too low is not true with concurrent writers. A
   compactor cannot use that assumption to authorize deletion.

## Proposed disk protocol

Use a versioned **mandatory compaction base**, distinct from optional snapshots.
Retain existing snapshot behavior for uncompacted directories. Do not change
consensus state-root, block-hash or transaction encoding algorithms.

The new `state.wal` begins with an explicit format header containing version,
network genesis hash, optional active recovery manifest hash, base digest,
height H, tip hash, and resume sequence S. Use magic whose first four bytes are
an invalid legacy frame length, so the strict old reader rejects it before
decoding entries. Continue ordinary frames after the header with absolute
sequences beginning at S; do not renumber committed frames. This requires
updating all strict readers, writer-open validation, repair/quarantine offsets
and recovery planners together. Legacy export tools must explicitly reject this
format until they support it; permissive prefix readers must not interpret an
unknown header as an empty source.

Base payloads are immutable, content-addressed files with their own version,
lengths and digest. They contain canonical ordered state, tip/recent blocks,
recovery/native inference context, pending inference records, rebase anchor,
and the exact provenance needed to restore every replay-derived index. Compare
all serialized state domains against independently replayed state before first
conversion; the account root alone does not authenticate every auxiliary row.
Retain the original authenticated ARCCHKPT and marker, and verify their network
and recovery-domain binding on every recovery-mode open. A local compaction
base is not a new quorum-certified checkpoint and must never be advertised as
one to peers.

Archive older history into immutable content-addressed chunks before retiring
its WAL records. The base references an authenticated archive index. Extend the
index with newly archived rows, without recopying the entire chain at each
compaction. Preserve existing historical lookup results; opening old history
can use a disk index instead of replaying obsolete account mutations. Any
future pruning policy needs an explicit separate decision. An archive preserves
information, so total data-directory size is not bounded independently of chain
history or live state. Only the mutation WAL is bounded.

Before enabling this on a node, inventory every writer and durable dependency.
If legacy DAG operations cannot be represented or independently recovered,
refuse conversion. Do not infer their retention from state height.

## Crash-safe transaction

First conversion is performed only on an approved offline copy. The eventual
online path needs a commit gate spanning all state mutations and snapshot
capture, plus the existing WAL admission gate. A WAL admission lock alone does
not freeze the in-memory maps. No code enabling either path is included here.

1. Quiesce mutations at a fully durable block/checkpoint boundary. Drain and
   fsync the writer. Capture (H, S, tip, state, recovery identity) under the gate;
   verify it against the committed boundary. For an online asynchronous capture,
   pin an immutable state view and retain every admitted frame with sequence
   greater than or equal to S, including later auxiliary entries tagged H.
2. Write base/archive temporary files with create-only, owner-checked,
   no-symlink APIs in the same filesystem. Check space before publication and
   enforce encoded size limits. Stream data instead of materializing a multi-GB
   WAL `Vec`. Fsync each completed file, publish its immutable name, and fsync
   the directory. Existing matching names must be verified, never overwritten.
3. Through the sole writer, drain and fsync again while admissions are gated.
   Write a new WAL temporary containing the header and the exact retained tail
   through that barrier. Verify checksums, contiguous sequences, next-block
   parent hash, checkpoint roots, and complete frame boundaries. Fsync it.
4. Atomically replace `state.wal`, then fsync the parent directory. On Windows,
   use the repository's write-through replacement primitive and explicitly test
   handle sharing and recovery of an interrupted namespace update. The old WAL
   is authoritative until replacement; the new WAL names only already-durable
   base/archive files. There is no separate mutable pointer to race the WAL.
5. Replace the writer's append handle before releasing admissions. Appending
   through the old, now unlinked inode would lose acknowledged commits. Latch
   any rename, directory-barrier, reopen or handle-swap failure as fatal to the
   writer; require restart instead of logging and continuing to sign.
6. Reclaim only unreachable files after the namespace barrier. Orphan staging
   and base files are ignored on recovery. Garbage collection follows references
   from the currently durable WAL generation, uses exact private filenames,
   and repeats safely after interruption. Never delete signing records, recovery
   packages, quarantines or arbitrary `.tmp` files as compaction garbage.

Crash reasoning: before WAL replacement, the complete old WAL still recovers;
after a durable replacement, the complete new WAL plus its durable base
recovers. Before the replacement's directory barrier, either namespace outcome
is acceptable because both dependencies remain intact. Missing/corrupt mandatory
dependencies fail closed; they never trigger genesis or ARCCHKPT-only fallback.
Subsequent compactions use new immutable base names, so an interrupted snapshot
publication cannot invalidate the previous generation.

Do not compact while a committed checkpoint is incomplete or the WAL already
has a latched error. Retain existing strict handling of complete corrupt frames
and quarantine only independently verified uncommitted/torn suffixes.

## Compatibility and rollback

**No: v0.8.11 cannot safely reopen the proposed compacted format.** This is a
disk-format migration even though ordinary WAL frame encoding, state roots and
block hashes remain unchanged. The new header is intended to make old readers
fail closed. The current candidate's prefix-removal and empty-WAL behavior is
tested here; old-binary rejection of the proposed format is not implemented or
tested yet. Do not label this draft STATECOMPAT-passed.

Required evidence: run the exact released v0.8.11 binary against original,
compacted, partially published and corrupted fixtures, with both normal and
active-recovery origins. Run the new binary against all supported historical
formats. Record executable hashes, fixture digests and full outcomes. Any old
binary that starts at genesis/transition rather than rejecting blocks release.

Maintain a verified pre-conversion directory on separate storage. A backup is
usable for binary rollback only while it represents the latest committed
boundary. After new commits/signatures, never rewind state or restore an older
signing record: replay/export all later commits into a verified legacy-format
directory or use an approved forward recovery procedure. If that conversion is
unavailable, in-place binary rollback is unsupported after compaction. A
forward fix is the available path. Verify the retained signing record and
consensus cursor against the final durable state before resuming signing.

The signing file is independently loaded and persisted
(`arc-node/src/consensus.rs:1638`, `:1669`). Its finality decisions, absence
observations and `last_applied_round` must remain identical. The next scan round
is last-applied plus one, including the distinction between `None` and `Some(0)`
(`arc-consensus/src/view_change.rs:510`, `:575`). Root equality is not a signing
behavior test.

## Required implementation tests

The tests in this draft characterize existing behavior, not crash-safe
compaction. The following gates remain open:

| Injection point or scenario | Required assertion |
| --- | --- |
| Partial base/archive write; file fsync failure | Old tip and all committed rows reopen; orphan ignored. |
| Each immutable-file rename and directory fsync, before and after | Old WAL reopens; it cannot depend on a partially published base. |
| Partial WAL header or tail write; WAL fsync failure | Old generation remains authoritative. |
| Immediately before/after WAL rename, before/after directory fsync | Model both persisted namespace outcomes; each recovers identical committed tip and state. |
| Writer reopen/handle swap; concurrent append at S | No lost, duplicated or renumbered acknowledged entries; late error latches, no further signing. |
| Every garbage-collection removal and namespace barrier | New generation survives; unrelated files and referenced archives untouched. |
| Second and later compactions interrupted | Previous mandatory base remains usable; no fixed-name snapshot dependency. |
| Missing/corrupt mandatory base, header, archive index or recovery identity | Explicit startup failure, never rollback to older checkpoint or genesis. |
| Both origins, auxiliary entries at H, rebase, native inference, receipts/logs, pruned signed indexes | Identical canonical state bytes, tip, all retained block hashes, historical lookup results and next-block hash. |
| Signing record roundtrip and conflicting finality/absence decisions | Byte-identical record, identical refusals and cursor; no repeated committed anchor. |
| Fixed-working-set long run, many compactions/restarts | WAL stays below configured byte threshold plus maximum admitted batch/block and header; bases/archives accounted separately. |
| Disk full, failed compaction, oversized base and concurrent readers | Preserve last durable generation; expose explicit failure/backpressure policy, never silently delete for space. |

Use subprocess termination hooks at actual filesystem operations plus a durable
filesystem model for power-loss outcomes; a returned injected error or SIGKILL
alone does not emulate loss of unsynced namespace changes. Run Linux and Windows
durability tests. macOS-only success does not establish the validator contract.

## Measurement protocol and outstanding input

An approved offline fixture must identify its origin, exact source binary and
configuration, accepted WAL boundary, height/root/tip, archive contents, recovery
manifest identity, signing-record digest and file sizes. Inspect keys neither
for generation nor for this benchmark. Run only on working copies.

Measure original and compacted copies with the same release-profile executable,
hardware and config. Record SHA-256 inventory, logical and allocated directory
bytes, peak temporary disk use, peak RSS, WAL validation/replay duration,
checkpoint activation and RPC readiness separately. Use multiple explicitly
labelled warm/cold runs, retain raw observations, and compare all state and
signing assertions before calling a run valid. A generated multi-GB fixture can
exercise scaling but cannot predict mainnet's state/history mix by byte size
alone. Do not pad a log with arbitrary repeated invalid entries.

Expected size must be measured as mandatory base + retained history archive +
tail WAL + recovery package + signing/DAG/other files. Peak conversion space
also includes new base/archive data and the temporary tail while the old WAL
still exists. The issue's 4.71 GB, 4.63 GB and 12.1 MB figures are insufficient
to calculate either result. **Post-compaction directory size and restart time:
not measured; no numerical forecast is provided.**

Local characterization commands (test temporary files may be redirected with
`TMPDIR`):

```sh
cargo test -p arc-state --test wal_compaction_preconditions --locked
cargo test -p arc-state --lib recovery_snapshot_is_not_a_base_for_state_wal_truncation --locked
cargo test -p arc-state --test snapshot_restore_equivalence --locked
```

Measured result on the development host: the full `cargo test -p arc-state
--locked` suite passes (448 tests, including 3 new integration tests, 1 new
recovery unit test, and 10 existing snapshot-equivalence tests). Those test
durations are not validator restart measurements. Compaction implementation,
fault injection, bounded-size run, mainnet-fixture measurements, STATECOMPAT
and Astra acceptance are still required before ENG-15 can be considered done.

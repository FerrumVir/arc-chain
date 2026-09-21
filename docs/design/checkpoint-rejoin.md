# Checkpoint rejoin (C11): design

Status: design, 2026-09-21. Not implemented. Scope: a validator whose peers no
longer hold the DAG rounds it needs - it was down longer than their DAG
retention window - and a node joining a chain whose round 0 is pruned.

## Why history transfer is not enough

A restarted validator rebuilds its DAG from its own durable commit cursor
(`ConsensusEngine::set_restart_base_round`). That works only while peers still
hold the cursor round. With the default 4,096-round retention at ~6 rounds/s,
that is about eleven minutes of downtime. Past it, the rounds whose anchors the
node never applied exist nowhere in DAG form, and replaying consensus is no
longer possible. The node must adopt the canonical state at a certified height
and resume consensus from there.

Two defects exist today:

1. A node with any canonical history refuses a verified checkpoint outright
   ("rebasing a durable store onto a checkpoint is not implemented").
2. The empty-node install applies the checkpoint to memory only. Nothing is
   written to the WAL, and the consensus cursor is not moved, so the node could
   not continue consensus from the checkpoint and would lose it on restart.

## What must be authenticated

The envelope today carries a quorum `FinalityCertificate` (height H, block
hash, state root, tx root) and the snapshot identity. The receiver verifies the
certificate against its own frozen committee and chain domain, the payload
digest, and - in a scratch state - that the payload reproduces the certified
state root. That authenticates the STATE at H.

Resuming consensus also needs the DAG position of H: the round of the anchor
whose commit produced block H. The commit cursor after adopting is
`anchor_round + 1`. A wrong cursor is a fork, not an inconvenience: the next
anchor this node commits would become height H+1 whether or not it is the one
its peers made H+1.

The round is authenticated without trusting the server:

- snapshot v2 carries the history of the last 256 heights, so the payload
  includes the tip block at H;
- its hash must equal the certificate's `block_hash` (so the header is
  certified);
- its header `proof_hash` is `state_decision_commitment(domain, anchor_hash,
  anchor_round)`;
- the envelope therefore gains `anchor_hash` and `anchor_round`, and the
  receiver recomputes the commitment and compares it with the certified
  header's `proof_hash`.

The other blocks in the window are authenticated by parent-hash linkage back
from the tip; the receiver refuses a window that does not chain.

## Durable adoption

A new WAL record, `WalOp::Rebase { height, tip: Box<Block>, state:
Box<SnapshotPayload> }`:

- written and fsynced BEFORE any in-memory change;
- replay: clear replayable state, install `state` through `apply_wal_op` (one
  implementation of what a record means), insert `tip`, set height H;
- WAL validation: a Rebase resets the validation state the same way and
  continues block/checkpoint contiguity and parent linkage from `(H,
  tip.hash)`; everything else is unchanged;
- history below the rebase point stays in the WAL (the node's own prefix is
  still the canonical chain's prefix); heights between the node's old tip and
  H - 255 are absent, an explicit archive boundary;
- an older binary cannot decode the new variant and refuses the WAL (fail
  closed). A rebased store requires this version; the release notes must say
  so.

A snapshot is published immediately after the rebase, so a restart does not
replay from far below it.

The same path replaces today's memory-only empty-node install.

## Consensus after adoption

- `ConsensusEngine::rebase_to(anchor_round)`: clears the DAG and round
  indexes, sets `last_committed_round = anchor_round + 1` and
  `current_round = max(current, anchor_round + 1)`, raises the proposal floor
  to at least the node's old restored round (anti-equivocation carries over),
  and opens the restart base round at the new cursor;
- the durable commit record (`ConsensusSigningRecord::last_applied_round`) is
  set to `anchor_round` and persisted before the engine moves;
- the signing record's finality votes and absence decisions are kept: they
  are about rounds and heights this validator already signed for;
- bootstrap then proceeds exactly as a restart past a short outage does.

## Safety argument (sketch)

- The adopted state is the certified state at H (root recomputed in scratch).
- The resume round is the certified header's commitment, so every honest node
  that committed H committed it at that anchor; the next anchor each of them
  scans is `anchor_round + 1`.
- Nothing below H is re-executed; nothing at or below the node's old
  proposals is re-signed (proposal floor), and its finality votes stand.
- A byzantine server can refuse, delay or offer a stale checkpoint (liveness);
  it cannot make the node adopt a state or a cursor a quorum did not certify.

## Tests required

- Rebase record replay: WAL with prefix + Rebase + tail equals the directly
  installed state; validation accepts it; a Rebase whose state does not
  reproduce its certified root is refused at open.
- Envelope: wrong anchor round / anchor hash / window linkage refused; tip
  block not matching the certificate refused.
- Engine: `rebase_to` clears, moves cursors, keeps the proposal floor.
- Process level: four validators with a 100-round retention; one killed for
  longer than retention; it adopts a checkpoint, resumes, agrees block for
  block and settles new work submitted through it; restarting it again
  afterwards recovers from its own WAL.

## Open questions

- Checkpoint freshness: serving nodes snapshot every 500 heights; the anchor
  round of a checkpoint must still be inside peers' DAG retention for the
  bootstrap that follows. Serve the newest snapshot, and have the requester
  retry for a newer one when its bootstrap from `anchor_round + 1` is refused.
- The duplicate-anchor index expects retention + 512 heights of history
  (`consensus.rs`); a rebased node has 256. Bound the index by what the node
  holds, or carry more window in checkpoints served for rebase.

# Checkpoint rejoin (C11): design

Status: **design v2, 2026-09-21. Not implemented.** The v1 design below
was drafted in code; an independent adversarial review found it unsafe (see
"Review of the v1 draft"), and v2 replaces it. The v1 draft must not be
committed. Scope: a validator whose peers no longer hold the DAG rounds it
needs (it was down longer than their DAG retention window), and a node
joining a chain whose round 0 is pruned.

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

## Review of the v1 draft (2026-09-21)

An independent read-only review of the uncompiled v1 draft found the
following. Each was re-checked against the code before being accepted.

1. **Only the tip is authenticated.** Window blocks were linked by their
   self-declared `hash`, never recomputed; `tx_hashes` were never checked
   against `tx_root`. Receipts, bodies and logs were installed with no
   binding. Worst, **on a chain without a recovery context the state root
   commits to accounts only** (`compute_state_root`: legacy account-only
   Merkle root; `compute_recovery_state_root` covers accounts, storage,
   contracts, identities, validators and the staking pool). So storage,
   contracts, identities, validators and staking in the payload were adopted on
   the server's word. Concrete attack: a forged window block carrying a later
   anchor's decision commitment makes the victim skip that anchor as "already
   applied" and fork silently.
2. **Unsolicited checkpoints were adopted.** The receive path never checked
   that a request was outstanding, so any validator could push one at a node
   briefly behind and reset its DAG.
3. **State moved durably before the engine could refuse.** `rebase_to` refused
   recovery-domain engines after the Rebase record was already fsynced,
   leaving state at H and the consensus cursor behind it.
4. **Live adoption and restart disagreed** on derived indexes
   (`pending_bond_releases`, `tier1_pending`, `native_inference_pending`, the
   community-reward activation height).
5. Smaller issues: a gap in receipt-based duplicate filtering on non-v3 chains,
   serving cost on the consensus thread, unbounded decode, a 16 MiB wire cap,
   a backward `rebase_to` accepted, and ignored `persist_signing_record` errors.

The review confirmed the tip chain itself (certificate → tip → `proof_hash` →
anchor → cursor), forward-only state movement, crash replay on the legacy path,
and the absence of any re-signing path.

## Where adoption can be authenticated at all

A checkpoint can be adopted only if the certified state root commits to
**every** domain the node adopts. That holds only on chains with a recovery
context (protocol-v3). It is also where adoption is needed: recovery-domain
consensus requires every validator in each round **unless an absence
certificate excuses it**, so a v3 chain keeps advancing while a validator is
down, and that validator can fall past retention.

On an account-only-root chain, which includes the private protocol-4 chains,
the root does not cover the other domains. Adoption is still possible there
under a stricter rule (v2.1, below), because a protocol-4 chain cannot change
those domains after genesis, and its native state is bound through fields of
certified accounts. Any other account-only chain whose payload differs from
the node in an uncovered domain is refused. The remedy there is a backup
restore (R6), or an owner decision to give new chains a full-domain root.

## v2.3: third review applied (not compiled)

A third independent review of v2.2 (read-only, 2026-09-22) found one high,
one medium and three low defects. Each was re-checked in the code before
fixing.

1. **High: account keys were not authenticated.** The account-only root
   hashes each account's contents in key order, not the key. A peer could
   file an account under another key between the same neighbours: the root
   still verifies, and the owner's account disappears on the adopting node,
   which then forks from its peers at that owner's next transaction. Now
   every adopted account must be filed under its own address
   (`accounts_bound_to_keys`), checked when planning, again immediately
   before the durable write, and again when a Rebase record is replayed at
   open. Honest state never files an account elsewhere (the native record
   validator already requires it).
2. **Medium: D20 had no activation.** Old and new binaries would select
   differently under a proposer packing used-nonce candidates, and a mixed
   network could split. The rule is now fixed per chain at activation and
   bound into the activation record and commitment (decision record D20).
   Existing chains keep rule v1 byte for byte; new activations use v2; a
   binary without D20 refuses a v2 chain at open. Adoption requires v2,
   because the defect D20 fixes applies to rebased nodes.
3. **Low: the checkpoint request did not close on every history advance.**
   The stalled branch moved the bootstrap watermark without closing it. It
   now closes wherever the watermark advances.
4. **Low: validator comparison depended on restart history.** Activation
   records only the members; each restart re-seeds the full genesis list, so
   entries below the minimum stake differ between nodes that restarted and
   nodes that did not. Only validators at or above the minimum are compared.
5. **Low: the rebase was applied without the native publication lock, and
   `rebase_onto_checkpoint` re-checked only the payload's shape.** The apply
   now holds the publication write lock (as activation and block execution
   do), and the adoption preconditions are checked again before the write.

Also fixed: an iterator call on a trait object that would not have compiled,
and three doc comments attached to the wrong items.

New tests: a relabelled account (asserting the root alone accepts it and the
plan and the rebase refuse it), a moved escrow refused by the scratch root
check with the WAL unchanged, tampered activation and pin rows, a changed
staking pool, a changed member stake, below-minimum registry entries
accepted, rule-v1 chains refusing adoption and keeping the counting rule, and
one activation layout and commitment per rule.

## v2.2: what is implemented (not compiled; second review applied)

This supersedes "Design v2" and "v2.1" above wherever they differ. A second
independent review of v2.1 found four more defects: duplicate rows, legacy
chains, receipt knowledge, and a gate that never closed. Each was re-checked
in code, and this is the result.

**Scope: protocol-4 chains only.** Adoption is refused unless native inference
is activated and there is no recovery context. On a legacy account-only chain,
contract storage and stake change without touching anything the root
certifies, so "equal to this node's" would adopt stale state. A recovery-
context chain's engine is repositioned only by its signed recovery path.

**What is adopted (`StateDB::plan_checkpoint_adoption`), and why each part is
bound:**

* **Accounts:** the certified account-only root, recomputed in a scratch store.
* **Rows a certified account commits:** a native escrow's metadata row (hash =
  the escrow account's `storage_root`) and the context pin row (= the pin
  account's `storage_root`). Such a row is adopted **only** if its certified
  account commits it. Equality with this node's copy does not count, because
  that copy may be the stale one.
* **Every other row, plus contracts, identities, validators, the staking pool
  and the reward activation height:** byte-identical to this node's own. On
  protocol 4 nothing but native inference runs after activation, so none of
  these can change.
* **Shape:** accounts, storage holders and each holder's rows are strictly
  increasing by key, so there is one version of anything. A row this node holds
  that the payload lacks is refused, since nothing certifies a deletion. So is
  a certified account with a new or changed `storage_root` whose committed row
  is missing.
* **History:** window blocks are re-hashed from the certified tip, with height
  keys, contiguity, `tx_count` and `tx_root` checked. Transaction bodies are
  **not** adopted (a body's hash excludes its signature), and neither are
  receipts, logs or the pending index.
* **Before any write:** the scratch store rebuilds the native pending index,
  which decodes every metadata row and checks it against its escrow. So nothing
  the post-write rebuild checks can fail after the record is durable.
  `rebase_onto_checkpoint` refuses any payload that still carries receipts,
  logs, bodies, a pending index or a recovery context, and so does the
  open-time check of a Rebase record.

**Receipt knowledge (new protocol-4 rule, decision D20):** a rebased node holds
no receipts below the checkpoint, and the commit path filters candidates by
receipts. `select_native_block_transactions` therefore skips any candidate
whose nonce its sender has already used **without counting it** toward the
64-candidate cap. On protocol 4 a transaction was applied exactly when its
nonce was consumed, so every node skips the same set whatever receipts it
holds. Without this rule, a leader packing 64 already-applied transactions
ahead of a live one would make the rebased node select differently from its
peers. The rule applies to every node, which is acceptable because protocol 4
is unreleased.

**Node:** a response is considered only while this node's own request is
outstanding, only from a peer it asked, and at most once per peer per request.
The request is closed as soon as history bootstrap makes progress or completes.
The node calls `check_rebase` before writing and panics (aborts) if anything
fails after the durable write; restart recovers from the record.

**Remaining limits:** serving still runs on the consensus thread. A state above
the 16 MiB wire cap cannot be served. Peers can feed stale but certified,
forward-moving checkpoints, which is bounded by solicitation. Live adoption keeps
its pre-rebase mempool and does not re-index `account_txs` (display only).

## v2.1 (superseded by v2.2 above)

`StateDB::plan_checkpoint_adoption` returns the only part of a payload a node
may adopt. Otherwise it refuses:

* **Covered by the root:** accounts.
* **Bound through a certified account:** a native escrow's metadata row, which
  hashes to that escrow account's `storage_root`, and the context pin row,
  which equals the pin account's `storage_root` (`native_storage_row_committed`).
  Every other storage row must equal this node's own. A row this node holds
  that the payload lacks is refused, because nothing certifies a deletion. So is a
  certified account whose `storage_root` is new or changed without the row it
  commits, because this node would later refuse a transition its peers apply.
* **Not covered, so must be unchanged:** contracts, identities, validators,
  staking pool, reward activation height and recovery context, each
  byte-identical to this node's own. Adopting them is then a no-op.
* **History:** window blocks re-hashed from the tip, with height keys,
  contiguity, `tx_count` and `tx_root` checked. Only bodies those blocks list,
  hashing to their key, are kept. Receipts and logs are dropped.
* **Derived indexes:** none are adopted. `rebuild_after_rebase` rebuilds the
  native pending index from certified storage (the admission height is the
  certified escrow account's nonce), plus Tier 1 and bond releases. Startup
  does the same, so live adoption equals a restart of the same WAL.
* **Refused outright:** recovery-context chains (the engine is repositioned
  only by the signed recovery path), and stores with a GPU account cache or a
  JMT root, which a rebase cannot rebuild.

The node accepts a response only while its own request is outstanding and
only from a peer it asked. It asks `ConsensusEngine::check_rebase` before
writing. It stops with a fatal error if anything fails after the Rebase record
is durable, and restart recovers from the record. It decodes peer payloads
within the 16 MiB wire cap and serves each peer at most one checkpoint per
30 s.

## Design v2

1. **Gate.** Adopt only when the local state has a recovery context and the
   engine has a consensus domain. Otherwise log why, and do not adopt.
2. **Solicited only.** Accept a response only while a request is outstanding,
   only from a peer it went to, and only after history bootstrap has proved
   unproductive (the condition that sends the request). Run every cheap check
   (tip, anchor, window) before decoding the state into a scratch store.
3. **Window.** For every window block: recompute the header hash; require the
   map key to equal `header.height`; require `tx_root` to equal the root over the
   carried bodies' hashes; require parent linkage by **recomputed** hashes. Do
   not adopt receipts or logs; they are unbound. The node has no receipts below
   H+1, which recovery chains tolerate because their duplicate filter is
   nonce/state based.
4. **No write before the engine agrees.** Add a pure
   `ConsensusEngine::check_rebase(anchor_round)`: it refuses a backward move and
   a cursor below the proposal floor. It must pass before the Rebase record is
   written. After the append, any failure stops the node with an explicit error.
   Restart then recovers from the Rebase record, so state and cursor cannot
   stay split.
5. **Recovery-engine rebase.** `rebase_to` works on a recovery engine. It
   keeps `recovery_bootstrap_round` (the domain's genesis round), opens the
   restart base round at `anchor_round + 1`, and raises the proposal floor. The
   recovery startup branch honours the durable rebase anchor as the legacy
   branch does. The post-recovery WAL validator validates Rebase records
   (forward move, tip self-consistency, root, linkage) instead of skipping them.
6. **One post-install routine** rebuilds every derived index. Startup replay
   and live adoption both call it, so a live rebase equals a restart of the
   same WAL.
7. **Bounds.** Serve at most one checkpoint per peer per interval, off the
   consensus thread; decode with an explicit size limit and no trailing bytes.
   Refuse to serve, with a log, a snapshot above the wire cap.
8. **Signing record.** Persist it before the engine moves; a failure is fatal
   like any post-append failure.

## Tests required (v2)

The v1 list, plus the review's missing negatives:

* a forged non-tip header whose `hash` field is self-consistent;
* tip `tx_hashes` that do not match `tx_root`;
* a non-account field tampered with while the account root is preserved (must
  be refused because account-only roots are refused);
* an unsolicited response;
* a recovery-domain node that cannot rebase refusing before any write;
* at open: a Rebase with a mismatched root, a backward Rebase, and a wrong
  parent after a Rebase;
* a crash between the append and the engine move;
* live-adopted state equal to reopened state for every derived index;
* the process-level test on a **recovery-context** fixture: four validators,
  one down past retention while absence certificates keep the rest going.

# Batch 3 isolated inference state adapter

Date: 2026-09-19
Repository: `arc-chain-readiness-20260919`
Scope: inactive state adapter only; no TxBody, RPC, live model route, or deployment wiring.

## Implemented

- Added [inference_contract_state.rs](../../crates/arc-state/src/inference_contract_state.rs#L1), an owned `IsolatedInferenceLedger` over persistent `StateDB`. It exposes account/root reads and `admit(request, input_blob, now)`, `finalize`, and `refund`; it does not expose the underlying state handle.
- Constructor requires a persistent WAL, rejects recovery-bound state, verifies authenticated `genesis.network-hash` against the inference domain, and requires frozen members to equal `StateDB::active_validators()`.
- Replaced independent model/profile/generation/assignment allowlists with bounded canonical `AllowedExecution` tuples. Commitment sorts/deduplicates complete tuples and admission requires exact tuple membership, avoiding unintended Cartesian combinations.
- Admission checks `input_blob` against `TIER1_INPUT_BLOB_MAX` and the signed `InferenceJob.input_hash` before account mutation; the blob is persisted in signed metadata and rechecked on load/replay.
- Added `WalOp::InferenceTransition` at the end of the enum, preserving historical discriminants. The record carries complete account replacements, bounded storage, context/member snapshot, signed request/input, and terminal receipt including full certificate evidence.
- Transition validates a complete serialized frame before append (256 KiB frame cap; 128 KiB metadata cap), exact escrow receipt storage, escrow storage-root commitment, status/output/certificate/input shape, and duplicate keys. It performs durable WAL barrier → private publication → checkpoint barrier. Any uncertain WAL error poisons the adapter.
- The trusted monotonic clock is the committed context account nonce. Every mutating transition replaces that account while preserving zero balance/code and context storage-root; finalize/refund reject backdated calls.
- Settlement skips valid zero share credits, preserves reserve conservation, and creates validated missing payee accounts at zero. Restart replays only checkpointed transitions, so a second-barrier failure rolls back the entire terminal transition.

## Validation

- `cargo test --locked -p arc-state inference_contract_state --lib`: **6 passed**.
  - admission → finalize → restart preserves balances, state root, input metadata, certificate evidence, and duplicate-finalize behavior;
  - expiry refund, wrong/oversized input, crossed execution tuple, wrong model/nonce paths preserve root;
  - backdated finalize rejection before and after restart across two requests;
  - price==reserve and tiny-price zero-share settlement;
  - maximum bounded input/output terminal settlement fits serialization limits;
  - first WAL barrier failure and second checkpoint barrier failure poison/restart safely; the second failure verifies escrow remains funded, validator payouts are absent, and pending metadata remains.
- `cargo test --locked -p arc-state persistent_restart --lib`: **5 passed**.
- `cargo test --locked -p arc-state wal_writer_reports --lib`: **1 passed**.
- `cargo check --locked -p arc-state`: passed; only existing vendor warnings.

## Remaining integration gates

This remains an inactive candidate adapter. Live activation still requires the future trusted model/profile registry and assignment admission, block/receipt integration, and protocol approval for the isolated economic policy. No production writes, network changes, commits, or pushes were made.


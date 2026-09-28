# Offline finality client

This standalone verifier depends by path on the pinned ARC source tree and does not modify it. It decodes the exact RPC JSON objects from `/finality/{H}` and `/block/{H}`, then uses `FinalityCertificate::verify` from `arc-consensus`; it contains no replacement signature or quorum implementation.

The operator-generated trust manifest must be independently constructed from the pinned host/genesis/recovery configuration. Pin the manifest's raw SHA-256 out of band and pass that digest on every invocation. `--host` is the capture host recorded by the evidence receipt; it must match the trust manifest. `--height` is the independently selected expected height and must be above the manifest checkpoint. Neither height nor committee is taken from the certificate.

The manifest schema is strict JSON:

```json
{
  "schema_version": 1,
  "host": "pinned-hostname",
  "chain_genesis": "64-hex-digits",
  "checkpoint_height": 0,
  "checkpoint_hash": "64-hex-digits",
  "domain_hash": "64-hex-digits",
  "recovery_epoch": 1,
  "validator_set_id": 1,
  "validator_set_epoch": 1,
  "validator_set_hash": "64-hex-digits",
  "total_stake": 0,
  "quorum": 0,
  "validators": [{ "address": "64-hex-digits", "stake": 500000 }]
}
```

The finality library commits to epoch and sorted address/stake membership; the helper derives tiers from stake and supplies a neutral shard slot because the finality verifier does not consume shard assignment. The manifest's raw digest pins all fields, including host/genesis/checkpoint provenance. A pass proves only that this certificate matches that pinned trust set and the paired block projections; it does not prove that the host served the bytes or independently validate how the operator established the trust manifest. Preserve capture receipts and hashes alongside inputs.

The helper enforces fixed bounds on all files, certificate hex, and validator count, uses ARC's limited exact bincode decoder, verifies the certificate against a caller-selected height/domain/set, checks RPC projections against the decoded certificate, checks the block hash against its header, and compares certificate block/state/transaction roots to the block. It ignores `verified_by_server` and other informational fields; only the independently checked payload matters.

Example after a remote CI build:

```sh
arc-finality-client verify \
  --trust trust-manifest.json \
  --trust-sha256 <pinned-manifest-sha256> \
  --host <capture-host> \
  --height <independently-selected-height> \
  --finality finality.json \
  --block block.json
```

Local compilation/tests are intentionally not part of the implementation pass because this workspace is under a shared-machine resource limit. The branch-scoped workflow seeds an isolated lockfile from the pinned workspace lock, then runs formatting, tests, and a release build remotely; it uploads the binary, resulting lockfile, and source/build receipt.

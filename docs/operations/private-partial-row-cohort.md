# Private fixed-residency row cohorts

This mode assigns disjoint output rows of every projection of one request to
operator-managed workers. It reuses the existing private pinned-SSH transport,
ARCROW01 worker identity, shared resident daemon, canonical integer arithmetic,
and strict backend. It does not add public worker participation or payments,
change chain admission/settlement, qualify a model, or establish a speedup.

Keep the existing `workers` entries and add an explicit top-level declaration:

```json
{
  "max_workers": 7,
  "duplicate_per_mille": 50,
  "spot_rows_per_stage": 2,
  "partial_rows": {
    "format": "arc.private-row-residency.v1",
    "manifests": [
      {
        "worker_id": "owner-0",
        "path": "/etc/arc/manifests/owner-0.json",
        "blake3": "<exact BLAKE3 of the manifest bytes>"
      }
    ]
  }
}
```

This is a schema fragment: list every worker and its distinct manifest pin.
The numbers illustrate existing policy settings; they are not performance or
capacity recommendations. Partial entries must leave `resident_layers` empty
and `resident_output` false. Both the configured maximum and the fixed backend
bound must accommodate every owner (currently at most 32).

Use exported rank/count bundles covering **all layers and LMHead**. Each worker
must own exactly one nonempty interval in every projection. The complete union
must be gap-free and nonoverlapping. Missing owners, duplicated identities,
manifest/model/profile mismatches, out-of-bounds rows, or bundles exceeding the
1 GiB serialized budget refuse configuration. Mixed layer-only and partial-row
workers are not supported in this first mode. No whole projection is replicated
on another worker merely to make it eligible.

Copy only small manifests onto coordinators. The manifest loader hashes their
bounded exact bytes, derives intervals from assignments, and validates declared
ARCROW01 sizes and payload digest fields. It does **not** read or authenticate
remote weight-file payloads. Start each partial daemon with the **same exact
manifest bytes and pin** held by its coordinator:

```
tensor_row_shared_worker serve --rows-dir /srv/arc/owner-0/rows \
  --artifact ARTIFACT_HASH --manifest /srv/arc/owner-0/manifest.json \
  --manifest-blake3 MANIFEST_HASH --socket /run/arc-owner-0/rows.sock
```

Both manifest options are required together. Startup verifies bounded manifest
bytes, the exact file set, identities and sizes; it hashes each file through the
same read stream that decodes the retained weights. A replaced payload of the
same size and header refuses startup. No separate prehash/reopen is trusted.
The service reports `verified_manifest_blake3` in its startup/shutdown stats.

Every partial SSH connection, including reconnects, requires an `ARCPIN01`
manifest/worker handshake and distinct `ARCPACK1` response before any warmup,
measurement or projection. Pinned daemons refuse unhandshaken clients; partial
clients refuse old or unpinned daemons. The existing relay carries these bounded
frames unchanged. Legacy full-layer clients and unpinned CLI remain compatible;
they do not gain payload pinning implicitly. Existing row-v1 frames, policy
hashes and consensus protocols are unchanged by this service handshake.

Preserve package/artifact qualification and runtime canonical numerical checks.
Pinned SSH authenticates the private host, not its software: a malicious trusted
host can falsely acknowledge a pin. This is configuration binding to verified
loaded bytes in the approved daemon, not remote attestation or a proof of
arbitrary public workers' honest computation. Manifest checks do not replace
the coordinator's full model-dimension/coverage validation.

`--native-low-residency` is mandatory. Warmup and measurement calls use rows inside
the worker's declared ranges. The challenge cache distinguishes layer/start/end.
Every owner must be connected, measured, unexcluded and reservable. Measured rates
never move its boundaries, and failure never enables full-model fallback.

Each owner's first row is checked by the coordinator on every projection, in
addition to the configured seeded stage spots. Every slice selected by
`duplicate_per_mille` is duplicated through the local canonical row source,
because another disjoint owner does not hold it. Checks stream bounded rows from
the qualified local artifact; they cost CPU, disk/cache traffic and latency.
The fixed planner publishes zero timing estimates (unavailable), and the cohort
view exposes `placement_timing_prediction_available: false`. Measure end-to-end
latency and aggregate host memory under all coordinators before making claims.

This changes assignment semantics explicitly: `ARC-private-fixed-resident-row-policy-v1`
and `ARC-private-fixed-resident-row-certificate-v1` are new domains. The private
audit envelope binds each manifest and uses inner certificate version 3, which
old verifiers reject. Existing full-layer JSON configs omit `partial_rows` and
keep their prior policy hashes, planners and v1/v2 certificate bytes.

Derive the actual policy without a model load:

```
arc-node native-assignment-policy --row-workers cohort.json --low-residency
```

The reported hash is configuration identity, not approval. Existing activation
allowlists must explicitly authorize that new hash through the normal binding
procedure; no old assignment hash is reinterpreted. No deployment or activation
is performed by this change. The earlier offline numerical proof does not by
itself qualify this production planner, its probes, SSH path or resource usage.

The additional offline integration gate uses the actual `RowCohort` low-residency
constructor, asynchronous probes/challenges, readiness, placement, reservations
and generation through ephemeral pinned loopback OpenSSH connections:

```
python3 scripts/qualification/run_low_residency_conformance.py \
  --binaries-dir /absolute/candidate/bin --model /absolute/pinned/model.gguf \
  --output-dir /absolute/new/proof-directory --row-partitions 7 \
  --production-cohort --kernel scalar --reference-kernel scalar
```

Build/package the `arc-node` example `production_partial_row_conformance` beside
the three existing inference proof executables. Run on an unprivileged fresh
Linux runner with OpenSSH client/server tools and `/run/sshd` available; missing
prerequisites refuse. The harness creates private ephemeral keys outside evidence
artifacts and pins the generated host public key directly. It starts only a
loopback SSH listener and removes its keys/processes during bounded cleanup.

The resident reference exits before row export/daemon loading. The production
report compares exact tokens/output hash and requires every owner measured and
certified, the expected remote call count, zero fallback/skips/faults and released
reservations. Production generation does not expose a per-position trace; the
earlier full logit/KV proof remains separate. Observation height `1` is explicitly
an offline constant, not a claimed chain clock. No qualification flag or native
admission context is created by this fixture. Seven logical owners on this one
host are not seven physical hosts or failure domains; per-owner declared RAM
does not enforce aggregate colocated residency. Paid-chain, actual fleet memory,
failure and WAN performance qualification remain separate requirements.

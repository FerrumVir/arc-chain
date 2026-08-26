# ARC testnet recovery notice

There is currently no supported “quick join” or validator-deployment command.
The public v2 fleet has mixed versions and divergent chain state, and the
v0.7.12 recovery candidate is not published or deployed. Do not pipe a script
from the mutable `main` branch into a shell, derive a validator identity from a
label, assign yourself stake, or treat one host's `/health` response as proof
that the network is live.

Community nodes are stake-zero observers. After an approved v0.7.12 release is
published with the complete checksummed headless asset matrix, follow the exact
tag-pinned process in [`../docs/HEADLESS_INSTALL.md`](../docs/HEADLESS_INSTALL.md).
That does not enroll the node as a validator or guarantee inference work,
rewards, or synchronization with a canonical chain.

Validator recovery requires a separately approved v3 fleet manifest. It must
pin binary and model digests, unique off-repository validator public identities,
the canonical genesis/checkpoint hashes, peer membership, verified host keys,
and one coordinated activation/rollback plan. The retired scripts under
[`../deploy/`](../deploy/) and the live-fleet operator scripts documented in
[`../scripts/README.md`](../scripts/README.md) must not be used.

## Supported local testing

The legacy shell launchers `scripts/create-testnet.sh`, `scripts/testnet.sh`,
and `scripts/run_cluster.sh` are retired. They generated fields rejected by the
v3 genesis/keyfile contract or launched seed-derived staked identities without
the explicit test authorization and approved genesis now required. They exit
before writing a key/config, starting a process, killing a process, or creating
state.

Use the Rust multi-node integration tests as the supported local harness. There
is no approved v3 shell launcher yet.

When diagnosing any purpose-built test cluster, report each process's exact version,
genesis hash, checkpoint/state root, peer set, and advancing finalized height.
An HTTP 200 or `{"status":"ok"}` by itself means only that one process can
answer HTTP; it is not evidence of shared consensus, absence of a fork, finality,
community inference assignment, or payment.

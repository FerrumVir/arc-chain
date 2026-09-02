# Retired v2 cloud deployment assets

The `deploy/` directory is historical reference material, not an approved way
to create, restart, monitor, or delete an ARC validator fleet.

The old path could download a moving, unchecksummed binary, generate identical
validator identities from labels, assign fixed validator stake, skip SSH host
verification, and call any HTTP response “healthy.” Those assumptions are
unsafe, particularly while the public v2 fleet is known to have mixed versions
and divergent chain state.

The recovery candidate is not published or deployed. There is no supported v2
cloud deployment procedure. The v3 recovery execution boundary is
[`scripts/recovery/recovery_rollout.py`](../scripts/recovery/recovery_rollout.py),
used only with the sealed manifest and evidence gates in
[`scripts/recovery/README.md`](../scripts/recovery/README.md). That manifest
pins every artifact by version and digest, binds a unique off-repository
validator identity to each host, pins the canonical genesis and checkpoint
hashes, uses verified SSH host identities, defines a coordinated
activation/rollback plan, and proves shared-chain progress rather than mere
HTTP reachability. The tool's presence does not make these retired cloud assets
operational or authorize an unsealed recovery run.

Current safeguards:

- `setup-testnet.sh` exits before reading credentials or contacting Hetzner.
- `monitor.sh` exits before querying a host or reporting network health.
- `teardown.sh` exits before looking up or deleting any server or local state.
- `cloud-init.yml` installs and enables nothing.
- `config/node-*.toml` are loopback-only, stake-zero retirement sentinels with
  no validator seed.
- `config/genesis.toml` remains fail-closed until approved validator public
  addresses and one coordinated activation plan exist.

Do not use any `deploy/Makefile` operational target or copy the reference
service/proxy files onto a host. The legacy shell testnet launchers are retired
too; use the Rust multi-node integration tests until an approved v3 local
harness exists.

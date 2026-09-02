# ARC benchmark safety boundary

The default benchmark binaries generate fresh Ed25519 identities from
operating-system entropy. Their account addresses therefore change on every
run; compare throughput and correctness metrics, not account IDs, across runs.

Predictable identities and deterministic transaction reconstruction are
available only with the nondefault `benchmark-tools` Cargo feature. This is an
isolated development facility, not an operator or testnet mode. In particular:

- the production/default `arc-node` binary has no `--benchmark` option;
- a feature-enabled benchmark node requires a positive-stake disposable
  no-genesis devnet and the explicit insecure-development identity flag;
- native RPC, P2P listeners, and every P2P peer must use numeric loopback IPs
  (`127.0.0.0/8` or `::1`); hostnames such as `localhost` are rejected; and
- community, shard, reward, and auto-join network targets are rejected, with
  no public-network override.

The networked multi-node harness is similarly feature-gated, uses fresh
process-local keys, and checks its typed listen and peer addresses before it
starts any transport task.

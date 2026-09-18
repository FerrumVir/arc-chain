# ARC protocol-v3 live recovery — deployment record (2026-09)

ARC Chain was recovered from the divergent legacy v0.7 fleet onto **protocol-v3** and is
**live in production**. This records what was deployed and how to operate it.

## Root of trust
- Quorum-signed recovery checkpoint at source height **138310** (recovery transition 138311),
  manifest hash `0x9c6aa3ec659b101751bf58a8a24c655f0e4fba79596e9b44f87a30f98f08acc0`,
  state_root `0xd103671a855f22414f4c9546eb329d6e1d1c5c1676781583900fe28622061c33`,
  5-of-6 validator signatures. Verified in-enclave (`recovery verify` RC=0) and activated
  per node via `recovery import` (`status: ACTIVATED`).
- Owner emergency-recovery approval: authenticated GitHub Actions run (owner FerrumVir),
  via `.github/workflows/owner-emergency-recovery-approval.yml` +
  `scripts/recovery/owner-emergency-recovery.py verify-github-artifact`.

## Cutover path (direct checkpoint import)
The production-rollout tool couples the cutover to the full v0.8.0 release provenance
(pre-tag artifacts, macOS canary, stage manifest), which was deferred. The chain was
instead brought live by importing the signed checkpoint directly into six fresh v3
data-dirs and starting the validators — no fabricated provenance.

## Live topology (6 validators, one per seed)
| city | host | stake | shard ranges |
|---|---|---|---|
| nyc | 149.28.32.76 | 6666667 | 0:6, 22:27, 27:32 |
| lax | 140.82.16.112 | 6666667 | 0:6, 6:12, 27:32 |
| ams | 136.244.109.1 | 6666667 | 0:6, 6:12, 12:17 |
| lhr | 104.238.171.11 | 6666667 | 6:12, 12:17, 17:22 |
| nrt | 202.182.107.41 | 6666666 | 12:17, 17:22, 22:27 |
| sgp | 149.28.153.31 | 6666666 | 17:22, 22:27, 27:32 |

Total stake 40,000,000; consensus quorum 26,666,667. Recovery epoch 1, validator-set-id 1.
Each seed runs: `arc-node-v3-<city>.service` (validator), `arc-caddy.service` (HTTPS
gateway), `arc-maint-interlock.service` (late-fork interlock), `arc-v3-firewall.service`.

## Public surface
- **Console + block explorer** (GitHub Pages): https://ferrumvir.github.io/arc-chain/ and
  `/explorer/`. Config `shared/frontend/arc-network.json` (state `recovered`, chainId
  `0x415243`, six HTTPS gateway sources, maintenance-interlock service). Published by
  `deploy-explorer.yml`, which runs a live-truth gate that fetches the nodes and verifies
  the checkpoint, the six-validator fleet, and the interlock before publishing.
- **HTTPS RPC gateways**: Caddy v2.11.4 on each seed, Let's Encrypt short-lived IP certs,
  reverse-proxying `127.0.0.1:9944` on 443; `/maintenance/status` routed to the interlock.

## Verified
- Six validators converged on one chain (identical blocks across all six), advancing well
  past the reopening floor 141191 (highest legacy fork head was 141063).
- Each node self-verified the signed checkpoint on startup.
- Console live-truth deploy gate passed against the live fleet.

## Known operational limitation
- **Slow restarts**: a node replays its full `state.wal` on every startup (no snapshot
  persistence; unauthenticated peer-snapshot bootstrap is retired), so restart time grows
  with height and this recovery epoch pauses production while a validator is down.
  Recommended follow-up: add state-snapshot persistence to arc-node so restarts load a
  snapshot + short tail. Until then, avoid non-essential validator restarts.

## Deferred (with the v0.8.0 release)
v0.8.0 GitHub release + 32 assets + installers/updater; native-Mac desktop gate;
post-release acceptance. Community reward canaries: enabling (validators loading model +
community-RPC as of 2026-09-18).

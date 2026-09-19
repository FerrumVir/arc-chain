# ARC explorer / node desktop readiness audit

Audit target: `/Users/excaulibur/work/arc-chain-readiness-20260919`, `main` at
`f616705` (2026-09-18). All HTTP checks were read-only; no transaction or
mutation endpoints were called.

## Proven live state

- `https://ferrumvir.github.io/arc-chain/explorer/` loaded in the Codex in-app
  browser. After refresh it displayed `ARC Testnet / RECOVERED`, “Canonical
  recovery verified”, six v3 replica identities, and advancing liveness.
- Browser block smoke: `#/block/138311` rendered canonical recovery boundary,
  exact parent hash to H=138310, block hash
  `d74321c2400fbb401b904b9e3c61cb751fd62fa7c9c7e0c23905d7c19159aae6`, and
  state root `d103671a...061c33`.
- Browser error smoke: `#/tx/not-a-hash` and `#/address/not-a-hash` rendered
  “Lookup failed” with the expected 32-byte hexadecimal validation errors.
- Direct HTTPS `GET /health` returned HTTP 200 on all six configured gateways
  (`149.28.32.76`, `140.82.16.112`, `136.244.109.1`, `104.238.171.11`,
  `202.182.107.41`, `149.28.153.31`), each reporting v0.8.0, five peers,
  six validators, and `chain_advancing:true`.
- `GET /network/info` on LAX reported chain `0x415243`, v3 identity matching
  the checked-in checkpoint, active stake 40,000,000, and block production.
  `GET /inference/attestations?limit=3` returned a valid empty evidence feed;
  no inference receipt or payment was fabricated.

## Fixed live gate compatibility issue

The repository explorer gate initially failed at `explorer/test-live.mjs:53`
because it required `GET /block/latest` and the deployed v0.8 gateway returned
HTTP 404. The reviewed source RPC still contains a `/block/latest` route, so
this is a deployment/gateway compatibility mismatch rather than proof that the
route is absent from source. The existing UI already fell back to `/info` and
`/stats` for height, which is why browser refresh succeeded.

I added a read-only `requestLatestBlock` fallback in
`[explorer/app.js](/Users/excaulibur/work/arc-chain-readiness-20260919/explorer/app.js:140)`
and `[dashboard/app.js](/Users/excaulibur/work/arc-chain-readiness-20260919/dashboard/app.js:91)`:
fallback is attempted only for HTTP 404, derives a safe height from status
endpoints, fetches `/block/{height}`, and rejects a height mismatch. Timeout,
TLS, auth, and other failures remain failures. The explorer contract now has
37/37 checks, including 404-only, unsafe-height, network-error, and
block-height-mismatch regressions. The dashboard contract has 37/37 checks
with the same fallback guards.

Commands/results:

```text
cd explorer
node ./test-contract.mjs                         # 37/37 passed
ARC_LIVE_CONFIG=../shared/frontend/arc-network.json node ./test-live.mjs
# PASS ... signed H=138310, verified H+1=138311, latest=632898

cd dashboard
node ../shared/frontend/test-arc-network.mjs    # 56/56 passed
node ./test-contract.mjs                         # 37/37 passed
ARC_LIVE_CONFIG=../shared/frontend/arc-network.json node ./test-live.mjs
# PASS observed once: fleet=healthy, replicas=6/6, common_height=632679
```

The dashboard live gate is intentionally strict and can fail during normal
height drift: two subsequent attempts observed all six reachable and agreeing,
but `drift=4`, yielding `recovered publication requires a healthy fleet`.
This is evidence of the existing `drift > 3` publication gate, not a reason to
relax it; the next successful run passed with the fleet within threshold.

The Sep 18 Pages failure (`checkpoint-evidence-incomplete`) is no longer
reproduced: the current six gateways return matching H/H+1 and v3 identity
evidence, and the explorer gate passes. That recovery-evidence result is
separate from the `/block/latest` 404 compatibility issue fixed here.

## Desktop readiness

`npm ci` completed cleanly in `desktop` (49 packages audited, 0
vulnerabilities). `npm run build` passed TypeScript and the Vite production
build; Vite emitted only existing dynamic-import and >500 kB chunk warnings.
`npm run test:unit` passed all 13 update-controller Playwright tests in 3.9s.
This proves the web bundle and updater-controller path on this host, but not a
native Tauri package or signed release. The v0.8.0 release, native-Mac gate,
signed installers/updater artifacts, and post-release acceptance remain
deferred (`docs/recovery/v3-live-recovery.md`).

## Remaining completion criteria

1. Publish/reproduce a complete signed v0.8.0 artifact set and pass the native
   Mac acceptance gate.
2. Re-run the strict six-replica dashboard/explorer gates against the intended
   gateway deployment and investigate any recurring drift above 3 rather than
   weakening the gate.
3. Demonstrate at least one successful canonical mined `0x25` reward receipt
   before claiming community earnings; current live inference activity is
   empty and rewards are disabled.
4. Resolve the documented slow-restart limitation (full `state.wal` replay)
   before treating validator/node operations as fully stable.

Unrelated `Cargo.lock` changes were already present from another agent/CI
activity and were left untouched.

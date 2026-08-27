# UI frames — video / design handoff

> **Historical visual snapshot only.** These populated frames and URLs do not
> prove current inference, shared chain state, community assignment, or payment.
> The 2026-08-26 read-only audit found the public fleet forked and
> version-skewed with community completed work at zero; v0.8.0 is not deployed.
> Do not use these images as a current product walkthrough. See
> [`../PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](../PRODUCTION-RECOVERY-AUDIT-2026-08-26.md).

Captured 2026-08-21 against the live testnet (height ~124,989, DAG round ~9.67M,
10 validators, 8 peers). Chromium, 1920×1080 viewport, `deviceScaleFactor: 2`,
so every PNG is 3840×2160 retina.

## Files

| File | What it is |
|---|---|
| `01-dashboard.png` / `-full.png` | Main dashboard, fully populated. **Best all-round frame.** |
| `02-wallet.png` | Wallet, empty "Get Started" state |
| `03-explorer.png` | Block explorer with live on-chain AI inference. **Best single frame for the story.** |
| `wallet-active.png` | Wallet with a funded account (older capture, use for the funded state) |
| `dashboard-inference-modal.png` | Inference detail modal |
| `dashboard-tx-modal.png` | Transaction detail modal |
| `dashboard-node-modal.png` | Node detail modal |
| `key-modal.png` | Key generation modal |

## Reproducing these

```bash
cd ~/arc-chain
python3 -m http.server 8899          # serve the repo root
node /path/to/shoot.mjs              # or just open the URLs in a browser
```

Live URLs (these are what to screenshare — they have real data):
- Dashboard — http://140.82.16.112:3200
- Wallet — http://140.82.16.112:3100

## Fixed while capturing these

`dashboard/index.html` had a null dereference in the stats refresh:
`getElementById('height')` and `getElementById('mempool')` both returned `null`
(no such elements in the DOM), which threw inside a `try { … } catch(e){}` that
swallowed the error. Everything after that line silently never rendered — the
header stayed on "Connecting…" and Validators / Blocks-per-sec / TPS / Total
Transactions / Accounts / Peers / Uptime all showed `-` even though `/stats`
was returning all of it.

Now routed through a null-safe `setText(id, value)` helper, and the `catch`
logs instead of swallowing. The dashboard renders fully populated.

## Known state at capture time

- SAO (São Paulo) and JNB (Johannesburg) seeds are **offline** — 6 of 8 up.
  Real, not a rendering bug. Either bring them up before filming or don't
  linger on that panel.
- Wallet loads in the empty state; use `wallet-active.png` for a funded view
  or fund an account before recording.

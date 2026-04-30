# ARC - pre-distribution checklist

**Target: 10,000 users. Zero silent failures.**

This document is the bill of work that stands between the current APK (which
boots, walks a user through onboarding, generates a real ed25519/BIP-39
identity, saves it to per-platform app storage) and a public release.

Read every line. Ship only when every **❌** becomes a **✅**.

---

## ✅ Done in this pass

- Real BIP-39 + chain-compatible key derivation (`src-tauri/src/identity.rs`).
  Seed phrase restores to the same address that `arc-node --validator-seed`
  produces. 6 Rust unit tests enforce determinism.
- Android APK builds via `npx tauri android build --debug --target aarch64`.
  Output: `src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`
  + `.aab` for Play Store. Includes `libarc_desktop_lib.so` for arm64-v8a.
- Android emulator launch verified (Pixel 2 API 33): app installs, welcome
  screen renders at 1080×1920, onboarding advances through hardware → role →
  identity. Identity screen shows real 64-hex address derived from a real
  12-word BIP-39 phrase.
- Store path fixed for Android. Was using `directories::ProjectDirs` which
  returns a read-only path on Android (`Read-only file system (os error 30)`).
  Now uses Tauri's `PathResolver::app_data_dir()` - resolves to
  `/data/user/0/network.arc.desktop/files` on Android.
- `reqwest` switched to `rustls-tls` - native-tls/openssl-sys can't
  cross-compile to Android without a pre-built OpenSSL.
- Responsive CSS: sidebar → bottom tab bar below 768px, 46px tap targets,
  info popovers become centered modals, `env(safe-area-inset-*)` respected.
- Logo swap slot at `src/assets/brand/` with README telling you which
  filenames to drop in. `LogoMark` uses SVG if present, falls back to CSS.
- Alpha-hardening (from the previous pass) still in: mock-strip, crash
  banner, port-conflict fallback, React error boundary, auto-updater.

---

## ❌ Blockers before a single APK leaves this machine

### 1. The app on mobile has no node to talk to

The desktop app assumes `arc-node` is running at `127.0.0.1:9090`. On a
phone, there's no local node and can't reasonably be one - iOS kills
background CPU after a few minutes, Android thermal-throttles a GPU
inference process, and neither OS lets you bind a port durably.

**You must decide now** what mobile does:

**(a) Companion/remote-control app** (recommended). Mobile points at the
user's desktop node (LAN) or a hosted gateway. Config gets a
`remote_host` field; `127.0.0.1` is the desktop default, `10.0.2.2` for
Android emulator, or an IP/hostname the user types in.

**(b) Verifier-only app**. Mobile validates attestation hashes against
the chain's public RPC (e.g. `testnet.arc.network:9090`). No local node.
Attestations show up, balance + faucet work.

**(c) Pretend it's desktop and watch users hit "Start node" → error.**
Don't do this. This is how you generate 10,000 1-star reviews.

I'd pick (b) for v0.1 mobile - lets you claim "10k verifier nodes" in
marketing, no infrastructure ask of the user. Point the RPC at a CDN-
fronted public gateway.

### 2. Hardware detection returns garbage on Android

The `sysinfo` crate on Android reports the JVM sandbox, not the device:
the emulator shows `0 CPU cores, 1 GB RAM`. On a real phone it will
report similarly useless numbers.

Mobile should **skip the hardware-detection step entirely** and pick a
sensible default role based on the OS (verifier on mobile, worker on
desktop). The `Hardware` screen in onboarding should be desktop-only.

### 3. No code-signing, no Play Store upload

The current APK is a **debug APK**. It will install via adb / "Unknown
sources" but:

- Not accepted by the Play Store (needs a signed AAB)
- Shows "For testing only" banner on some Android versions
- Users must enable "Install unknown apps" per browser/email app

Before production:

```bash
# Generate upload keystore (one-time, save offline + encrypted)
keytool -genkey -v -keystore ~/arc-upload.jks \
  -keyalg RSA -keysize 4096 -validity 10000 \
  -alias arc-upload

# Paste the keystore password + alias into
# src-tauri/gen/android/key.properties (gitignored):
#   storeFile=<absolute path to arc-upload.jks>
#   storePassword=...
#   keyAlias=arc-upload
#   keyPassword=...

# Build signed release AAB
npx tauri android build --target aarch64 --apk --aab
# Output: app-universal-release.aab
```

Then: Play Console → Create app → upload AAB → fill privacy policy →
internal testing track → review (~1–3 days) → production rollout.

### 4. No Play Store metadata

Required before upload:

- App name: "ARC" (check trademark conflict - there's an "Arc" browser)
- Short description (80 chars)
- Full description (4000 chars)
- 2 screenshots (min), ideally 8 - one per screen
- Feature graphic (1024×500)
- App icon 512×512 (regenerate from your real logo, not the bitmap
  wordmark currently in `src-tauri/icons/`)
- Privacy policy URL (live, accessible, mentions: address storage,
  seed-phrase handling, network calls to testnet.arc.network)
- Data safety form (what data you collect, sold? shared?)
- Content rating questionnaire
- Target audience + content guidelines

### 5. Fake node state when the app can't reach a node

If RPC is down, the dashboard currently shows zeros everywhere and the
attestation feed is empty. There's no "I can't reach the network"
banner. Add one. Users will assume the app is broken.

### 6. Identity import flow is wired but has no UI

`commands::import_identity` is wired (`State<_, AppState>.lock().await`)
but the Onboarding wizard doesn't have a "Restore from phrase" option.
Users who reinstall cannot get their address back. **This is the second
most likely way to lose users' funds.**

Fix: add a "Restore existing identity" button on the Welcome screen →
textarea for the 12 words → `validate_bip39` → `import_identity` →
straight to dashboard.

### 7. No Android UI tests

Playwright tests cover the *same* React code the APK ships, but they
run in chromium at a mobile viewport - not in Android WebView. Subtle
differences (scroll behavior, font rendering, keyboard handling, WebView
version differences across OEMs) aren't caught.

Minimum: drive the APK through onboarding on one real device per OEM
you care about (Samsung, Google, Xiaomi, OnePlus). If you ship without
this, plan for a 1.0.1 patch day-one.

### 8. No Sentry / crash reporting

10k users = ~50 crashes on day one, distributed across 30+ device
models. Without crash reporting you'll hear about maybe 2 of them via
app reviews. Wire `sentry-android` (JVM side) + `sentry-rust`
(native side).

---

## Suggested release timeline

| Week | Work |
|---|---|
| 1 | Scope decision (companion vs verifier), implement chosen path, add Restore flow, Sentry, network-down banner, skip-hardware-on-mobile |
| 2 | Real device QA on 5+ Android devices, Play Store metadata, privacy policy, Apple Developer enrolment for iOS |
| 3 | Internal testing track on Play Store, 10–50 testers, iterate |
| 4 | Closed testing track, 100–500 testers, iterate |
| 5–6 | Open testing → production rollout with staged % (1% → 10% → 50% → 100%) |

**Do not push to 10k at once.** Play Store lets you stage rollouts by
percentage so a critical bug only hits N% of users before you pull it.
Use that.

---

## If you want the minimum path to 10k this week

Ship **desktop-only** to 10k (Mac first since that's where the APK-less
setup cost is zero - everything is ready except Apple Dev ID signing).
Keep Android in closed testing until scope decisions above are made.

Desktop blockers remaining: `Apple Developer Program enrolment ($99, ~24h)`
+ `tauri.conf.json bundle.macOS.signingIdentity` + `APPLE_ID env var for
notarization` + `updater pubkey` (one-time `npx @tauri-apps/cli signer generate`).

That's 2–3 calendar days of Apple waiting + ~4 hours of config work.
Real test device requirement: your own Macs + 2–3 borrowed ones running
different macOS versions (13, 14, 15).

10k to desktop, staged over a week, with the alpha-gate improvements
already shipped, is a reasonable product. 10k to Android this week is
a reputation risk.

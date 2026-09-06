# macOS arm64 pre-tag community-worker canary

This runbook exercises the exact protected-preflight `arc-node-macos-arm64`
bytes **before** the final tag exists. It is an operator canary, not an
installer and not an updater. `plan` is read-only and `install` writes local
files but does not load or start a process. Only `start` allows the worker to
register, heartbeat, claim work, and return results to the six reviewed ARC
HTTPS origins.

The helper refuses a release download, unpacked directory, local receipt, or
hand-renamed binary as authorization. Its input is the raw exact-ID GitHub
Actions ZIP for the macOS arm64 headless group. Every `plan` and `install`
invocation freshly queries the public GitHub REST API with a hash-pinned,
root-owned curl and explicit hash-pinned CA bundle. It proves protected current
`main`, the exact completed/successful `workflow_dispatch` run and attempt, and
the exact unexpired artifact ID/name/server digest/size before opening the ZIP.
It then privately verifies both archive layers, exact membership,
`BUILD-METADATA.json`, and every payload hash. `install` repeats the live API
proof after all large copies and immediately before publishing runnable state.

## Safety boundary

- Native Apple Silicon macOS only (`Darwin` / `arm64`), as a non-root logged-in
  user with a `gui/<uid>` launchd domain.
- RPC is fixed to `127.0.0.1:19944`; EVM RPC is disabled.
- Each process proof also enumerates the exact PID's listening TCP sockets and
  requires the sole listener to be `127.0.0.1:19944`. A wildcard, external,
  alternate, or additional listener fails the canary. During `start`, once the
  executable and full argv have been proved, such a listener also disables the
  label and gracefully quarantines the exact process with `SIGTERM`; it never
  leaves a rejected listener eligible for restart and never force-kills it.
- A stake-zero community worker has no consensus/P2P role, even with a complete
  production genesis. The proof therefore requires **zero UDP sockets**. Any
  UDP socket fails closed; opening an otherwise-unused QUIC transport would
  contradict the node's role separation rather than prove readiness.
- The node runs with `--stake 0 --community-mode --full-integer-worker`, no
  `--peers`, no seeds file, and `--p2p-port 0`. It takes no consensus role and
  opens no QUIC socket, so it does not expose an inbound public P2P listener or
  require a public IP, firewall rule, or port forwarding.
- The only community endpoints are six repeated `--community-rpc-url`
  arguments: `https://149.28.32.76`, `https://140.82.16.112`,
  `https://136.244.109.1`, `https://104.238.171.11`,
  `https://202.182.107.41`, and `https://149.28.153.31`.
- The local GGUF must be exactly 4,081,004,224 bytes with SHA-256
  `08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa`.
  The source must be a single-link, operator-owned regular file below
  non-writable, non-symlink ancestry. `install` reads it through one stable
  no-follow descriptor and publishes an independently hashed, create-only,
  mode-`0400` managed copy. The helper never downloads, replaces, or deletes
  the operator source.
- The candidate genesis must be the reviewed recovered genesis, SHA-256
  `8394894aaf32aff64df5c6988186e4802cb77a62daf259d8f5cab11d818ed269`,
  with all six validators and reward activation height 137146.
- A dedicated Ed25519 key is created at
  `~/.arc-pretag-community-canary/identity/community-worker-ed25519.json`.
  It is an owned, non-symlink mode-`0600` file. Private key bytes never appear
  in the LaunchAgent, runner argv, config, evidence, stdout, or node log; only
  the keyfile path and public address do.
- Managed programs/config are published create-only and bound by SHA-256 and
  size in `config/canary.json`. A mismatched existing file is an error, never
  an overwrite. The runtime argv uses only the managed GGUF; the runner checks
  its owner, mode, link count, size, and SHA-256 again immediately before
  `exec`.
- Every `install`, `start`, `status`, `stop`, and `cleanup` transaction holds
  the same persistent mode-`0600` no-follow lifecycle lock in the operator's
  home directory. Cleanup never unlinks or replaces that lock, so concurrent
  controllers cannot revive or orphan a LaunchAgent mid-transaction.
- Lifecycle and process proofs invoke only the fixed root-owned macOS paths for
  `id`, `uname`, `launchctl`, `ps`, and `lsof` under a fixed minimal
  environment. Caller `PATH`, `DYLD_*`, and preload variables cannot select or
  modify those proof tools.
- launchd starts the runner through SIP-protected `/usr/bin/env -i`, so the
  unsigned candidate node never inherits GUI-domain loader interposition,
  `BASH_ENV`/`ENV`, language-runtime hooks, proxy settings, custom TLS/OpenSSL
  configuration, or an SSH agent. Only fixed `HOME`, private managed `TMPDIR`,
  system `PATH`, C locale, and `RUST_LOG=arc=info` cross the exec boundary; the
  runner defensively unsets the same hook families again before executing the
  hash-proved node. The exact root-owned `/usr/bin/env`, `/bin/sh`, `stat`,
  `shasum`, and `cut` paths are proved before lifecycle operations.
- `start` recognizes only the same launchd PID moving through the exact
  `/usr/bin/env` invocation, exact `/bin/sh <managed-runner>` invocation, and
  final hash-bound node invocation. The env/shell runner and an initializing
  node must own no TCP or UDP sockets. The final node must own exactly the
  loopback RPC listener and no UDP sockets. Any argv/executable near miss, PID
  change, or premature socket fails closed.
- The proof window is bounded at 300 seconds. This includes re-hashing the 4 GB
  managed GGUF immediately before `exec`, loading its full integer model, and
  opening RPC. A trusted phase that does not become ready in that window is
  disabled and left loaded for review without a signal or bootout.
- The LaunchAgent is not `RunAtLoad`, has no `KeepAlive`, has no updater, and
  sets `ExitTimeOut=4420`. Before status or shutdown, the controller proves the
  exact launchd PID, executable path and hash, and complete argv.
- If a kickstarted job never exposes a provable PID, `start` disables the label
  but deliberately leaves the loaded job intact for review. It does not issue
  `bootout` across a racy no-PID observation that could hide a just-starting
  process.
- If an earlier failed proof left an already-running exact node loaded but its
  label disabled, a later `start` first repeats the complete PID, executable,
  argv, hash, TCP, and zero-UDP proof. Only then does it re-enable the label,
  re-prove the same PID, and append explicit recovery evidence. It never
  kickstarts or replaces that running process.
- `stop` disables the label, re-proves the same process, and asks launchd to
  send only the node-supported `SIGTERM`. It waits up to 4,420 seconds for the
  admitted 4,000-second work window and WAL barrier. A dedicated drain proof
  keeps requiring the same launchd PID, executable, hash, and complete argv.
  Its listening TCP set may be exactly the sole loopback RPC listener or
  empty, UDP must remain empty, and RPC may never reopen after the first empty
  observation. Already-established local RPC or outbound HTTPS connections may
  finish during the bounded drain; no new listener is permitted. Closing RPC
  is not treated as process death or WAL completion.
  The controller waits for both launchd ownership and the original PID to
  disappear, then takes three one-second-apart observations plus a final
  recheck requiring the exact loaded definition to report `active count = 0`,
  `state = not running`, and `last exit code = 0` before `bootout`.
  It never sends a force signal and never boots out a live process.
- If an enabled loaded job has no exact provable PID, the first `stop` disables
  it and leaves it loaded for review; it neither signals nor issues `bootout`
  across that racy observation. A later `stop` can recover only an already
  disabled, cleanly exited job whose exact path, program, ProgramArguments,
  working directory, log paths, umask, and launchd definition remain stable.
  It requires three one-second-apart `active count = 0`,
  `state = not running`, no-PID observations plus a final recheck, scans the
  process table for the exact canary env/runner/node commands, revalidates all
  installed bytes, and requires the normalized definition hash to remain
  unchanged. Only then does it unregister the inert label without signaling
  and append explicit recovery evidence. Any PID, process, definition, state,
  or exit-code change leaves the disabled job loaded for review; cleanup
  consequently preserves the LaunchAgent plist as well.

Runtime paths must contain no whitespace or shell metacharacters because the
controller compares macOS `ps` output to one unambiguous complete argv. Move a
model or materialized candidate to a simple absolute path before planning if
needed.

## Materialize the exact selected preflight bytes

Start from the reviewed successful `release-signing-preflight.yml` run on the
exact protected `main` commit. These values are evidence inputs; do not select
"latest" and do not infer an attempt.

```bash
export ARC_PREFLIGHT_COMMIT='<40-lowercase-hex-main-sha>'
export ARC_PREFLIGHT_RUN_ID='<positive-run-id>'
export ARC_PREFLIGHT_RUN_ATTEMPT='<positive-run-attempt>'
export ARC_PREFLIGHT_VERSION='0.8.0'
export ARC_PRETAG_SELECTION='<exact pretag_artifacts JSON from the validated run>'
export GH_TOKEN="$(gh auth token)"

scripts/release/verify-pretag-run-and-artifacts.sh \
  FerrumVir/arc-chain \
  "$ARC_PREFLIGHT_COMMIT" \
  "$ARC_PREFLIGHT_RUN_ID" \
  "$ARC_PREFLIGHT_RUN_ATTEMPT" \
  "$ARC_PRETAG_SELECTION"
```

Download only the immutable artifact ID selected for the macOS arm64 headless
group, preserve the response as the raw ZIP, and make it mode `0400`. Do not
unpack or rename content. The canary's shared verifier performs the independent
digest, size, archive, membership, metadata, and payload checks itself.

```bash
export ARC_CANARY_WORK="$HOME/arc-canary-preflight"
umask 077
mkdir -m 700 "$ARC_CANARY_WORK"

export ARC_CANARY_ARTIFACT_ID="$({
  printf '%s' "$ARC_PRETAG_SELECTION" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["macos-arm64"]["headless"]["id"])'
})"
export ARC_CANARY_RAW_ACTIONS_ZIP="$ARC_CANARY_WORK/headless-macos-arm64-actions.zip"
gh api \
  -H 'Accept: application/vnd.github+json' \
  "repos/FerrumVir/arc-chain/actions/artifacts/$ARC_CANARY_ARTIFACT_ID/zip" \
  > "$ARC_CANARY_RAW_ACTIONS_ZIP"
chmod 400 "$ARC_CANARY_RAW_ACTIONS_ZIP"

export ARC_CANARY_CURL='/usr/bin/curl'
export ARC_CANARY_CA_BUNDLE='/private/etc/ssl/cert.pem'
export ARC_CANARY_CURL_SHA256="$(/usr/bin/shasum -a 256 "$ARC_CANARY_CURL" | /usr/bin/cut -d ' ' -f 1)"
export ARC_CANARY_CA_BUNDLE_SHA256="$(/usr/bin/shasum -a 256 "$ARC_CANARY_CA_BUNDLE" | /usr/bin/cut -d ' ' -f 1)"
```

The artifact work directory and source model are inputs and are never cleaned
by the helper. The independently verified managed model copy is also preserved
by cleanup. Keep them until release evidence has been reviewed.

## Plan, install, and start

Set the canonical local model path. `plan` hashes the full GGUF, so it can take
a little time; it performs no filesystem or network mutation.

```bash
export ARC_CANARY_MODEL="$HOME/.arc-models/llama2-7b.gguf"

scripts/release/macos-community-canary.py plan \
  --raw-actions-zip "$ARC_CANARY_RAW_ACTIONS_ZIP" \
  --model "$ARC_CANARY_MODEL" \
  --expected-commit "$ARC_PREFLIGHT_COMMIT" \
  --expected-run-id "$ARC_PREFLIGHT_RUN_ID" \
  --expected-run-attempt "$ARC_PREFLIGHT_RUN_ATTEMPT" \
  --expected-artifact-id "$ARC_CANARY_ARTIFACT_ID" \
  --curl "$ARC_CANARY_CURL" \
  --curl-sha256 "$ARC_CANARY_CURL_SHA256" \
  --ca-bundle "$ARC_CANARY_CA_BUNDLE" \
  --ca-bundle-sha256 "$ARC_CANARY_CA_BUNDLE_SHA256"

scripts/release/macos-community-canary.py install \
  --raw-actions-zip "$ARC_CANARY_RAW_ACTIONS_ZIP" \
  --model "$ARC_CANARY_MODEL" \
  --expected-commit "$ARC_PREFLIGHT_COMMIT" \
  --expected-run-id "$ARC_PREFLIGHT_RUN_ID" \
  --expected-run-attempt "$ARC_PREFLIGHT_RUN_ATTEMPT" \
  --expected-artifact-id "$ARC_CANARY_ARTIFACT_ID" \
  --curl "$ARC_CANARY_CURL" \
  --curl-sha256 "$ARC_CANARY_CURL_SHA256" \
  --ca-bundle "$ARC_CANARY_CA_BUNDLE" \
  --ca-bundle-sha256 "$ARC_CANARY_CA_BUNDLE_SHA256"

scripts/release/macos-community-canary.py start
scripts/release/macos-community-canary.py status
scripts/release/macos-community-canary.py accept
```

`start` succeeds only after launchd, `ps`, and `lsof` prove the exact PID,
executable, hash, size, argv, sole loopback TCP listener, and absence of UDP.
It can remain quiet for roughly two minutes while the exact runner hashes the
managed model and the node loads it; the hard proof deadline is 300 seconds.
While it runs, read local status without exposing the RPC listener:

```bash
curl -fsS http://127.0.0.1:19944/health | python3 -m json.tool
curl -fsS http://127.0.0.1:19944/node/info | python3 -m json.tool
```

`accept` repeats the complete installation and live-process proof and then
create-only seals
`~/.arc-pretag-community-canary/evidence/ACCEPTED.json`. That canonical receipt
binds the dedicated worker address to the protected commit/run/attempt,
artifact ID and digests, exact node/model/genesis bytes, six HTTPS coordinator
origins, stake-zero/full-integer argv, and proved PID/listeners. Keep the worker
running: the production manifest must be built within six hours of this
timestamp, and its reward probes target this exact address. A retry accepts only
byte-identical already-sealed evidence.

A live process or a `degraded` isolated-chain health classification is not a
reward claim. Record actual registration, assigned work, mined `0x25` receipt,
and earnings evidence separately before accepting the release canary.

## Stop and cleanup

```bash
scripts/release/macos-community-canary.py stop
scripts/release/macos-community-canary.py cleanup
```

`cleanup` is intentionally narrow: after a proven graceful stop it removes
only the exact LaunchAgent plist and leaves the label disabled. It does **not**
delete the GGUF, dedicated key, chain data, node log, installed candidate, hash
bindings, or append-only evidence under
`~/.arc-pretag-community-canary/evidence/`. Archive the evidence before making
the final tag decision.

If any PID, executable, argv, owner, mode, hash, size, metadata, genesis,
model, key identity, or launchd state differs, the command fails closed. Do
not bypass that error with `kill`, `launchctl bootout`, or a file replacement;
preserve the state for review.

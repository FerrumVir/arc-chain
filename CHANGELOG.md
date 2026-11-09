# Changelog

All notable changes to ARC Chain are tracked here. This project follows
[semantic versioning](https://semver.org/).

> **Backfill note (2026-08-17).** Entries for v0.7.1 through v0.7.11 were
> reconstructed from `git log` in a single pass; this file had stopped at
> v0.7.0. They are terser than the hand-written entries below.
>
> **Release-topology caveat.** The version on the seeds, the version with a
> git tag, and the version with a downloadable binary are three different
> things right now:
> - **v0.7.10 and v0.7.11** were shipped **desktop-only** via
>   `release-desktop.yml`, so they carry Tauri bundles and **no `arc-node` CLI
>   asset**. v0.7.7 is the newest release with a CLI binary.
> - **v0.7.8 and v0.7.9 have no release and no tag.** The five non-NYC seeds
>   run a binary built out-of-band from branch `fix/v078-attestation-wire-compat`,
>   which was merged to main only on 2026-06-16 (f6bee03).
> - **Nothing on the live network runs v0.7.11.**

## v0.8.19 - Unreleased

- Coordinators prefer the fastest waiting worker using their own accepted-result
  timing per model position. New workers get a bounded share of assignments;
  a preferred worker can hold a queued job for at most 1.5 seconds.
- A quickly declined job can be retried once on another waiting worker when
  the dispatch budget allows it. This needs no worker update.
- Validators can check committed output tokens in one pipelined pass, with the
  same authenticated shard quorums and repetition penalty as sequential
  generation. Unsuccessful checks fall back to sequential verification.
- Workers send exact output tokens only when the coordinator advertises support.
  Verified tokens can also accelerate capable reward approvers without changing
  stored settlement journals or reward signatures.
- Coordinators probe committee identities before starting ordinary reward
  approval rounds and defer rounds that cannot reach five distinct validators.
  Recovery probes retain their existing scheduling.
- Increase shard queue headroom for overlapping verification passes while
  keeping the compute concurrency limit unchanged.
- The community worker processes the prompt in batches (same tokens and hash).
  `POST /node/threads` now answers only direct calls from the same machine.

Desktop app:

- A computer that agreed to share compute now keeps trying to finish setting
  up. If the 4 GB model download stalls or fails, the app tries again every
  20 minutes and soon after the computer wakes from sleep, instead of waiting
  for the next launch or a click. It resumes from the bytes already saved. It
  stops once the computer is contributing, compute is turned off, the
  computer has less than 16 GB of memory, or the disk lacks room for the rest
  of the download.
- These background retries download only on mains power, and not on a
  network the system reports as metered (read on Linux through
  NetworkManager; Windows and macOS are treated as unmetered for now). A
  download you start yourself is never held back.
- A contributing computer whose model file went missing or became unreadable
  now downloads it again by itself, at launch and in the background.
- Only one setup attempt runs at a time. A click while a background attempt
  is running waits for that attempt and shows its result.
- Updates arrive sooner: the app checks every 6 hours, when its window is
  shown or focused, and after the computer wakes, with at most one such
  check every 15 minutes. These checks are timed from the native app, so a
  window hidden in the tray still runs them.
- An automatic update that is waiting for an AI job to finish now tries again
  every hour for as long as the job keeps it waiting, instead of giving up
  after three tries.
- The one-time "Share your computer's spare power" question is now asked
  only once the window is visible. A launch that starts hidden in the tray
  at login no longer uses it up without anyone seeing it.
- Verified work whose reward is waiting for validator approval now reads
  "Verified. Reward pending validator approval." instead of saying no ARC
  reward can be confirmed.

## v0.8.18 - Release-preparation snapshot (2026-11-08)

> **Candidate note:** This entry records the v0.8.18 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.18.

- Network questions could be echoed or answered with an empty response because
  the pinned Llama-2-7B-Chat model received plain text instead of its chat format.
  Requests with `chat_template: true` now use `[INST] ... [/INST]` once at the
  coordinator for that exact model, including community dispatch, local fallback,
  sharded inference and consensus inference. The public demo is unchanged (#240).
- Requests with `chat_template: false` (or no flag) are unchanged. Other model
  artifacts retain their existing behavior (#240).

- Very short chat questions may receive fewer output tokens to preserve older
  desktops' timeout headroom; requests that cannot fit even one output token
  ask the caller to request more tokens or add words (#240).

## v0.8.17 - Release-preparation snapshot (2026-11-04)

> **Candidate note:** This entry records the v0.8.17 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.17.

- Transport writes are bounded and never hold the peer map across a write.
  Broadcast, directed-send and heartbeat-probe writes take a handle to the
  peer's send stream, release the peer map, then lock the stream and write
  under a timeout of 5 s plus 1 s per MiB of payload. A write that times out
  or fails removes that exact connection, and a write cut off mid-frame
  marks the stream so no later frame follows it. A write that waits behind a
  longer healthy one on the same stream, such as a large history or
  checkpoint response, has its own wait budget; when that runs out it writes
  nothing and leaves the connection alone (#224).
- When the consensus outbound queue stays full, the node samples it once per
  tick: it logs a warning after 30 s of continuous backpressure and once a
  minute after that, and after 120 s it logs an error and the consensus
  thread exits the process with status 1, so systemd restarts it (the units
  `install.sh` writes restart a node that exits with an error). While a
  parent-first replay window waits for room, inbound messages are moved,
  unhandled and in order, into a bounded local queue, so the transport's
  peer readers keep reading instead of waiting on a full channel (#224).
- SIGTERM is handled on its own thread. The node receives it on a dedicated
  operating-system thread with its own current-thread runtime, not as a task
  on the main runtime. After SIGTERM, if the main runtime runs no task for
  60 s, the process exits with status 1. A shutdown that keeps making
  progress keeps the full service stop timeout (#224).
- Community result verification now runs when a model layer range has
  exactly two live validator replicas. The coordinator recomputes such a
  range on both replicas, and both must return the same authenticated hash
  (and, on the last range, the same token): a 2-of-2 quorum. A range with
  three or more live replicas is still verified 2-of-3, exactly as before,
  and a range with one or none still cannot be verified. Every quorum still
  needs two distinct, authenticated, active-validator signatures. Rewards,
  issuance caps and the five-of-six approval rule are unchanged, and nothing
  stored on chain or checked when blocks are applied changes. Each approving
  validator recomputes the job on its own node, so approvals for a job
  verified 2-of-2 need the approving validators on this release (#232).
- The `verification` object in `/inference/run` and `/community/submit_work`
  responses says which quorum ran. `method` stays
  `authenticated_shard_quorum_2_of_3_per_range` when every range was verified
  2-of-3, and the object is then the same as before. It is
  `authenticated_shard_quorum_2_of_2_per_range` when every range was verified
  2-of-2 and `authenticated_shard_quorum_2_of_3_or_2_of_2_per_range` for a
  mix. `replicas_contacted_per_quorum` is the fewest replicas any range
  contacted, and a new `ranges_verified_2_of_2` field lists those ranges as
  `[start_layer, end_layer]` pairs; it is omitted when empty. Twin receipts
  use the same labels in `validator_recompute.method` (#232).
- `GET /inference/readiness` reports `community_dispatch_ready: true` only
  while a community result could be verified: every layer range of the
  validator shard pipeline needs at least two distinct live replicas, read
  from the same shard view the verifier uses. While it is false,
  `live_community_workers` is 0, and `safe_to_dispatch` is true only when this
  node can still answer from its own model or the validators' sharded
  pipeline. The body keeps its nine `arc.inference.readiness.v1` fields. A
  new `x-arc-community-dispatch` response header says `ready` or why
  community dispatch is not: `model_identity_unavailable`,
  `no_eligible_workers`, `work_queue_unavailable`,
  `verification_pipeline_unavailable` or `verification_replicas_below_quorum`
  (#232).
- `POST /inference/run` no longer sends a job to a community worker when its
  result could not be verified. It answers at once from this node's own model
  or the validators' sharded pipeline, as when no worker is eligible, instead
  of holding the request until the dispatch deadline. A sealed recovery probe
  in that state is refused at once with 503, and a public demo request gets a
  503 that names verification as the reason. The optional community demand
  pump (`--community-demand-pump`, off by default) is not gated by this check
  (#232).
- `scripts/recovery/community-reward-probe.py` still requires 2-of-3
  verification on every range and position. A result verified with a
  two-replica quorum is now refused with that reason, instead of "omitted
  authenticated shard-quorum verification" (#232).
- Desktop setup starts with Contribute compute on when the computer meets
  the memory requirement for the ARC model, the same check that marks the
  model Recommended; below it the step starts off, as before. The step is
  titled "Share your computer's spare power", a Contribute compute switch
  sits under its text, and Skip still turns it off. With it on, setup no
  longer waits for the 3.80 GB download: the node starts as an observer,
  then the promotion path a consented install already takes at launch reuses
  or downloads the model, checks its pinned SHA-256 and restarts the node as
  a worker. A download that does not finish continues the next time ARC
  starts the node at launch. A setup that turns compute on stores a core cap
  of a quarter of the logical cores, rounded down and at least one, which
  the node gets as `RAYON_NUM_THREADS` and `--threads`. A core count already
  stored is kept, a re-run of setup keeps it, and a Settings save no longer
  overwrites it. Existing installs keep their settings and consent. Settings
  states the share that applies. The wording of the step and of the Settings
  note is in the next entry (#231).
- The desktop setup step and the Settings note now say what the node does
  while compute is on, and nothing more. The step reads "ARC Node can run AI
  jobs for the ARC network on this computer. It uses up to a quarter of your
  processor, at low priority so your own apps come first, and about 8 GB of
  memory while it is ready for jobs. It keeps running on battery. First it
  downloads the ARC model, about 4 GB, in the background. You can turn this
  off here or at any time in Settings." The Settings note for a quarter of
  the processor reads "Uses up to a quarter of your processor at low
  priority, and about 8 GB of memory while ARC Node is running. It keeps
  running on battery. Turn this off to stop ARC jobs and free that memory."
  A note for another core count says the same about priority and memory. The
  wording says the node runs while ARC Node is running: closing the window
  hides ARC to the menu bar or system tray, and the node keeps running there
  until ARC is quit. The step for a computer below the memory requirement
  states the same facts. Nothing in this release waits for idle time, heat
  or free memory, and none of that is promised (#234).
- The node the desktop starts now runs at low operating-system priority, so
  the person's own apps get the processor first when it is busy:
  BELOW_NORMAL_PRIORITY_CLASS on Windows, a nice value of 10 on macOS and
  Linux, set before the node executes so every thread it creates inherits
  it. This is scheduling only. The core cap still limits how much of the
  processor the node may use, and nothing pauses it, on battery or
  otherwise. If the system refuses the change the node still starts, and the
  log says it did not get low priority (#234).
- An install that finished setup before compute started on by default, has
  compute off, and meets the memory requirement is asked once, the first
  time ARC opens after the update: a dialog with the setup step's title and
  body, and two buttons, Turn on and Not now. ARC records that it asked in a
  marker file beside `store.json` before the dialog shows, so quitting with it
  up never asks again, and a rollback that rewrites `store.json` does not
  reset it. Turn on stores consent and, when no core count is stored, a
  quarter of the logical cores, then downloads the model in the background as
  setup does; Not now and Escape change nothing. It is not shown to an
  install that has just finished setup, which asked the same question, to one
  that already has compute on, to one that answered the v0.7 upgrade question
  (which comes first), or to a computer below the memory requirement. A
  launch where the question does not apply records the marker too, so
  turning compute off later does not ask (#234).
- Home's node strip says "Your node is getting ready: downloading the AI
  model" with the amount downloaded while the model downloads in the
  background with the node already running, as it does after setup and after
  Turn on. It used to say so only while the node was off. A download that
  reports nothing for 2.5 minutes without a final report, because it failed
  or the computer slept, is no longer shown, and the next report brings the
  line back (#234).
- The generated release notes no longer say that updates never download or
  install without confirmation, which stopped being true in v0.8.14. They say
  that unattended installs have been on by default since v0.8.14 and that
  turning off "Install signed app updates automatically when idle" in
  Settings asks first. The release-contract check now fails if the notes
  claim updates wait for confirmation, or name a Settings switch the app does
  not show (#234).
- Settings → Identity no longer says the app doesn't store the recovery
  phrase. The app keeps the identity, phrase included, as plain text in
  `store.json` in its `network.arc.desktop` data folder on this computer,
  and never sends it over the network. The card now says that, what the
  system can still copy (backups that include the folder, a Windows roaming
  profile, a clipboard that kept the phrase), and to write the phrase down,
  keep it offline and keep the computer secure (#223).
- The setup step for contributing compute shows what that means before setup
  goes on: what runs, when jobs can run (whenever the node runs, including
  from the menu bar or system tray after the window is closed, and after
  login while Start node on app launch is on), power use, and that
  keep-awake is off unless it is switched on in Settings. Every setup step
  scrolls when the window is shorter than the step, so its action stays
  reachable at the 960x640 minimum window (#223).
- Ask ARC's help describes the path a prompt takes. It no longer says
  prompts go to your own node first: the app reads the chain from one
  coordinator for the whole session, and the prompt goes to that
  coordinator's `/inference/run`, which offers it to a community worker
  first and, if none takes it, to ARC's own nodes. Your own node takes the
  prompt only if no coordinator has answered the app yet this session, or if
  the app reads the chain from your node (#223).
- Release tooling: an update of an existing chain can be attested with one
  validator declared offline. By default all six validators must be live and
  on the candidate, as before. In the exception, the owner's input names
  exactly one validator (its site, IP address, validator identity and stake)
  as declared offline and confirms it again when the attestation is
  assembled and when the producer is dispatched. The five live validators
  must hold at least the quorum stake and each run the selected binary in
  both samples, and the declared validator's public health read must show no
  answer. A configured validator that does not answer is never moved to the
  offline set, and the claim authorizes no validator retirement (#233).
- Security: `deny.toml` records RUSTSEC-2026-0429 (a denial of service in
  the `zip` crate's `extract()`) as a reviewed exception. The node's zip
  1.1.4 has no symlink branch in `extract()` and candle-core 0.8.4 never
  calls it; the desktop's zip 4.6.1 compiles only on Windows and is reached
  only for a signature-verified update zip, and the updater binds Windows to
  the signed `setup.exe` asset. The exception is removed no later than the
  v0.9.0 dependency review. The desktop lockfile moves async-executor to
  1.14.1, because 1.14.0 was yanked for a use-after-free when a Waker fires
  during `Executor::drop`. CI now runs cargo-deny on the workspace, the
  desktop and the updater verifier and reports every failing graph, not just
  the first, and audits the TypeScript SDK's shipped runtime dependencies at
  low severity and its dev-only test tooling at high (#221).
- Verification: a cross-platform determinism proof for the pinned
  Llama-2-7B-Chat Q4_K_M model. A workflow runs 16 fixed prompts of 32 new
  tokens each through the canonical INT8 profile on macOS (Apple Silicon and
  Intel), Linux x86_64 and Windows x86_64, with the scalar kernel and the
  SIMD kernel, and requires every run to produce a byte-identical
  transcript. In the recorded run all 8 platform and kernel pairs matched.
  The workflow runs only on a pull request that touches its files and
  carries the `determinism-proof` label, or by dispatch from main, so normal
  CI is unaffected. `docs/determinism-proof.md` has the measured results and
  the steps to reproduce them (#136).
- Documentation: the README, Getting Started and the headless guide state
  that v0.8.16 is released, give a dated status (2026-11-02) and no longer
  call it unpublished. The sections written for the 2026-08-31 snapshot
  move, unchanged, under a dated History heading that says they are
  superseded. The README and Getting Started say that signed app updates
  have installed automatically by default since v0.8.14, that Settings can
  turn that off, and that on macOS v0.8.16 defers an update it can't apply
  where ARC runs; they also add the Discord and Telegram links. The README
  performance section and both papers carry a dated scope note: the GPU
  bit-identity, speed, throughput and node-count figures are historical lab
  and testnet results, and the current proven scope is
  `INFERENCE_DETERMINISM.md` (#222).

## v0.8.16 - Release-preparation snapshot (2026-10-26)

> **Candidate note:** This entry records the v0.8.16 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.16.

- macOS: Install update no longer leaves ARC on its old version with the
  node stopped when ARC runs from somewhere its updater can't replace it:
  the mounted .dmg, a copy macOS runs from a temporary read-only location
  (App Translocation, for an app opened where it was downloaded), or a
  different drive from the temporary folder. Tauri's updater moves the
  running app aside before it puts the new one in place, and only a
  "permission denied" refusal makes it ask for an administrator; any other
  refusal failed the install after ARC had stopped the node, and every
  reopen of the same copy offered the same update again. ARC now checks
  where it runs before either update path stops the node or takes its
  update lock: Install update keeps the update available, and an
  unattended install is deferred, both with "ARC can't update itself from
  where it's running. Quit ARC, drag ARC Node into your Applications folder,
  then open it from there." The node keeps running. Home shows the same
  sentence as one calm line from launch. An Applications folder you can't
  write to is not refused: the updater asks for an administrator there, as
  before (#218).
- While an update ARC couldn't finish holds the node, which lasts until ARC
  quits, Start said "another ARC desktop currently owns the managed node
  lifecycle … Resource temporarily unavailable (os error 35)": it tried to
  take the lock ARC's own update was holding. Restart was refused with
  internal wording. Both now refuse with "ARC couldn't finish an update.
  Quit ARC and open it again.", and the Dashboard offers Quit ARC in place
  of Start and Restart, as Home's node strip already did (#218).
- Node memory: the in-memory block table now stores each block behind a
  pointer, so a bucket costs 17 bytes (a height, a pointer and a control
  byte) instead of 313. The table keeps every block the node holds. It is
  split into shards, and each shard's bucket array doubles when 7/8 full,
  with every bucket sized for an entry whether filled or not: at 2^24
  buckets in all (up to 14,680,064 blocks) the arrays took about 5,008 MiB,
  and the next round of doublings, one per shard and all near the same
  height, would have added another 5,008 MiB. That round now adds about
  272 MiB. Each block is a separate allocation of its own size, so memory
  still grows with the blocks held. Reads return the same blocks as
  before, and nothing on disk or on the wire changes (#220).

## v0.8.15 - Release-preparation snapshot (2026-10-23)

> **Candidate note:** This entry records the v0.8.15 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.15.

- The desktop can send a short anonymous report when the node won't
  start, crashes, gets stuck for 10 minutes or an update fails, and one
  when it recovers, so problems get fixed without screenshots. A running
  app also sends one small heartbeat a day. Reports carry a random install
  ID, the app and node versions, the platform, the failing step, a short
  error code and the error text with home folders, emails, IP addresses,
  wallet addresses and keys removed. Nothing else is sent: no wallet
  addresses, keys, recovery phrase, peers, logs or prompts. It is on by
  default, shows a one-time notice before anything is sent, and can be
  turned off in Settings → Error reports, which deletes anything still
  waiting; See what's sent shows the last five payloads exactly.
  `desktop/PRIVACY.md` lists every field (#209).
- Release builds send those reports to
  `https://arc-node-care-intake.onrender.com`; a build without a report
  address sends nothing (#210).
- Windows: the desktop no longer refuses to start the node with "DACL
  inherits from its parent" on `.arc-node.lock`. On launch it repairs that
  one file's permissions (you, SYSTEM and Administrators only) and still
  refuses files owned by anyone else and links; `arc-node` now creates the
  lock with private permissions in the first place (#211).
- Dashboard text, error banners, logs and dialogs can be selected and
  copied again, and the node status on Home has a Copy details button
  (#211).
- Ask ARC no longer waits out the whole compute budget when the network
  cannot serve a question. A coordinator now withdraws a community
  assignment that no worker has claimed within one 30 s claim window, and
  one whose worker has stopped heartbeating for longer than the 90 s
  registry TTL, and answers from its own sharded pipeline instead of
  holding the caller for up to 65 minutes and then returning 504. A
  withdrawn job can no longer be submitted, so no late worker result can
  race the fallback. Two trade-offs: a worker whose heartbeats to a
  coordinator fail for 90 s while it is computing loses that job and its
  reward, where v0.8.14 still paid it if the result arrived in time; and
  under a burst, questions queued behind one busy worker fall back after
  30 s instead of being served in sequence. Workers that keep
  heartbeating, submits already under verification, twin execution and
  recovery probes are unaffected (#214).
- A healthy community node reads as calm status instead of errors. Such a
  node never has peers (the network admits only validators as peers) and
  reaches the chain through a coordinator, so its normal state showed as
  "Lite", "Syncing", "Running · 0 peers", three Dashboard banners ("HTTP
  client mode", "Client mode" with a Reset peer state button that could
  not help, "Connecting to peers") and a red node strip for a node that
  is off or an update that is ready. Every screen now says Connected,
  Connecting, Node off or No connection in the same words, with one plain
  line on the Dashboard; a node that is off is not red. Ask ARC says
  "Asking the ARC network…" with the time elapsed and, when no answer
  comes, one sentence ("The network didn't answer this time.") with the
  returned text behind Show details and Copy details. The crash banner,
  wallet errors and update errors take the same shape; a transfer or
  faucet claim whose answer was lost after it was sent says to check the
  balance before sending again, since the host may have included it. The
  tray no longer says "syncing · 0 peers" (#215).
- Windows: a manual upgrade through the installer's "Uninstall before
  installing" choice runs the old version's uninstaller, whose "Delete the
  application data" checkbox deleted `%APPDATA%\network.arc.desktop`: the
  wallet key, the recovery phrase and the node identity. The node's data in
  `%USERPROFILE%\.arc\data-v3` stayed behind, so the new install came up with
  no identity next to an earlier install's node data. When that node had
  already stopped without a clean shutdown, launch could not run the one
  recovery launch that clears its receipt (it needs an identity), and every
  Start was refused with "managed-node durability recovery failed: no
  identity - run onboarding" until ARC restarted; with the node still
  running, it was drained and a fresh identity then ran on the old data.
  Launch now recognises node data left by an earlier install whose app data
  is gone (the desktop's own shutdown token or executable record is there,
  and no identity store existed when ARC started; a store that is present
  but cannot be read is left alone, and never written over), renames it
  beside itself (`data-v3.previous-install-<UTC>`, nothing deleted) once
  nothing live holds it, starts fresh, and says so in one sentence during
  setup and on Home: "ARC found node data from an earlier install and set
  it aside (<path>). If you had a wallet, restore it with your 12 words;
  your node will sync again from the start." A directory a running node or
  another ARC desktop still holds, a linked path, or any doubt leaves it in
  place and runs the previous launch checks unchanged. The
  v0.8.15 Windows uninstaller asks "Delete anyway?" before deleting app
  data, says that the ARC wallet keys go with it and that the 12-word
  recovery phrase brings the wallet back, and keeps the data by default in
  silent and passive runs. A v0.8.14 → v0.8.15 manual upgrade still runs
  v0.8.14's uninstaller, so leave its "Delete the application data" box
  clear; the launch fix above covers an install where it was ticked (#217).
- Security: tokio 1.53.3 in the node, CLI and desktop (RUSTSEC-2026-0386,
  `AsyncFd` constructors that violate I/O safety). RUSTSEC-2026-0387
  (crypto-bigint 0.5.5, timing variability) is recorded as a reviewed
  exception in `deny.toml` until the k256 0.14 migration, no later than
  v0.9.0: validators, the desktop and every node-side signer use Ed25519,
  and secp256k1 signing is reached only by user-held keys in `arc-cli`
  (#216).

## v0.8.14 - Release-preparation snapshot (2026-10-20)

> **Candidate note:** This entry records the v0.8.14 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.14.

- A refusal to start the node no longer blocks the desktop's own signed
  app update. A desktop stuck behind one, such as v0.8.12's start deadlock
  after an unclean node exit, can now install the release that fixes it
  from inside the app. The update replaces only the app: it never starts,
  stops or replaces arc-node, never touches the node's shutdown receipt,
  and waits while the launch-time check is still running. Node starts,
  the recovery start and node-binary replacement stay refused. A refused
  v0.7 data migration still needs a manual install (#205).
- Signed app updates install automatically by default. Desktops from
  v0.8.13 and earlier are switched on once, including where it had been
  turned off; Settings can turn it off again, and that choice is kept. Linux
  .deb and .rpm installs still update through their package manager. This
  takes effect from v0.8.14 on: v0.8.12 and
  v0.8.13 desktops still need one click (Settings → Install) or a manual
  install to reach it (#205).
- A node that can't start is reported as one plain sentence with one
  action (Install update, Restart ARC or Try again), with the full message
  under Show details and Copy details. Problems the user has to repair, such
  as a path, a permission, a migration or another ARC desktop holding the
  node, keep their details open (#205).
- ARC opens on Ask ARC, the AI chat, which works whether or not this
  computer's node is running: a question goes to the ARC network when the
  local node can't take it. One line above it says what the node is doing
  (starting, downloading the AI model, off, stopped unexpectedly, blocked,
  needing a repair, updating, an update that needs ARC reopened, or a new
  version ready) with the one button
  that fixes it, so an available update now shows on the home screen, not
  only in Settings. If the node can't start during setup, Continue to ARC
  finishes setup and opens Ask ARC, with Try again beside it. The recovery
  phrase step and the choice to run AI jobs are unchanged (#205).
- The legacy bridge pins arc-node v0.8.13 (bridge release v0.7.14), so v0.7
  installs that update move to v0.8.13 instead of v0.8.12 (#204).
- Security: rustls-webpki 0.103.16 (RUSTSEC-2026-0382, certificate
  verification could accept distrusted intermediates), and webpki-roots and
  webpki-root-certs 1.0.10 (RUSTSEC-2026-0381, -0383, bundled roots that
  trusted distrusted emSign intermediates), in the node, CLI and desktop.
- Security: the desktop's build tool vite moves to 8.2.4
  (GHSA-698v-4r3x-3vcv, GHSA-r825-w8mm-5pxw, GHSA-587h-7wmj-h5pm, in the
  editor launcher of vite's development server, which is not part of the
  shipped app) (#207).

## v0.8.13 - Release-preparation snapshot (2026-10-18)

> **Candidate note:** This entry records the v0.8.13 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.13.

> **If your desktop is stuck at "Couldn't start arc-node":** an install
> that shows "managed-node durability recovery failed … startup
> reconciliation is still in progress" cannot reach this release through
> the in-app updater, because update preparation waits behind the same
> check. Quit ARC from its menu-bar or tray icon (**Quit ARC Node**),
> download v0.8.13 from its GitHub release page, and install it over the
> current app (on Windows, run the installer; on Linux, replace the
> AppImage). Its first launch finishes the interrupted shutdown, and the
> node then starts as usual. Nothing needs to be deleted: the wallet and
> the node's data stay where they are.

- The desktop starts its node again after the previous node stopped
  without ARC stopping it: a crash, a forced quit, or the computer
  restarting, logging out or losing power while the node ran. On the next
  launch ARC runs one recovery start of that exact node, so the node can
  replay its data and acknowledge a clean shutdown. From v0.8.0 to v0.8.12,
  ARC's own launch-time check refused that recovery start. Start, Retry,
  setup and in-app updates then failed on every launch (#202).
- Real refusals still block every start, the recovery start included: a
  legacy-data migration that is not durable, an invalid shutdown receipt,
  or a recovery that fails. ARC never moves the node's data directory and
  never points v0.8 at preserved v0.7 data (#202).
- While the launch-time check runs, Start and Retry say to try again in a
  few seconds instead of giving repair instructions. An invalid shutdown
  receipt is reported as itself, instead of leaving "still in progress" on
  screen for the rest of the session (#202).
- rustls is 0.23.46 in the node and desktop lockfiles, for
  RUSTSEC-2026-0346, -0347, -0348 and -0349 (#202).
- `deny.toml` records owner-accepted exceptions for RUSTSEC-2026-0367,
  -0368 and -0369 (zip) and RUSTSEC-2026-0344 (ruzstd). No release of the
  crates that pull them in accepts the patched versions yet (#202).
- `deny.toml` also records owner-accepted exceptions for the
  RUSTSEC-2026-0370, -0371 and -0372 maintenance notices (zvariant,
  zbus_names and zvariant_derive). Upstream merged those crates into
  zbus 6. The desktop reaches them only through tauri-plugin-opener on
  Linux and the BSDs, and no tauri-plugin-opener release accepts zbus 6
  yet. For every exception the file gives the reasons and the removal
  conditions, no later than the v0.9.0 dependency review (#202).
- *Backfilled 2026-10-20:* these changes shipped in v0.8.13 but were
  missing from this entry.
  - The legacy bridge pinned arc-node v0.8.12 and moved to bridge release
    v0.7.13, so v0.7 desktops could reach the v0.8 app through
    `latest.json` (#199).
  - A missing or unreadable model file no longer leaves the desktop's node
    unable to start. The node starts as an observer with local compute
    off, the dashboard says why, and Settings offers **Re-download model**.
    Recovery starts take the same check, and a recorded node that has
    provably exited no longer blocks startup (#200).
  - quinn-proto is 0.11.18 in the node and desktop lockfiles, for
    RUSTSEC-2026-0338, -0340 and -0343. The desktop's previous 0.11.16 was
    also inside -0339, -0341 and -0342 (#201).

## v0.8.12 - Release-preparation snapshot (2026-10-11)

> **Candidate note:** This entry records the v0.8.12 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.12.

- Workers and validators now run the exact fast INT8 kernel by default:
  AVX2 on x86-64 and NEON dot-product on arm64 (an x86-64 process that
  Rosetta 2 translates on Apple Silicon stays on the scalar kernel). The
  switch is process-wide, so community-worker generation, shard holders and
  validator recomputes all use it. It computes exactly the values of the
  scalar kernel and refuses any input it cannot compute exactly to the
  scalar kernel before writing a byte (#173). To go back to the scalar
  kernel set `ARC_CANONICAL_KERNEL=scalar` (or `ARC_FAST_CANONICAL_KERNEL=0`,
  the older name, which is still honoured); `simd` and `auto` are also
  accepted, and an unrecognised value selects scalar and logs a warning.
- Behind the fast kernel a node uses the widest exact kernel its CPU has:
  AVX-512 VNNI, then AVX-VNNI, then AVX2 on x86-64, and SMMLA (i8mm), then
  SDOT on arm64. They serve one-token decode and multi-row projections with
  the same checks and the same results (#190). Set
  `ARC_CANONICAL_BATCHED_KERNEL` to `neon-sdot-limb`, `neon-i8mm-limb`,
  `avx2-limb`, `avx-vnni-limb` or `avx512-vnni-limb` to pin one kernel; a
  pinned kernel the CPU lacks falls back to the best one it has. The node
  logs its kernels when it loads a model and reports them, with the pin
  status and each kernel's projection count, under `kernels` in
  `GET /node/threads` and `GET /node/info`.
- Community jobs run on the node's configured inference thread pool, so
  `--threads`, `[inference] threads` and `POST /node/threads` apply to them.
  With no setting nothing changes (#173).
- Public error responses carry generic reasons for journal and
  shard-pipeline failures: no server paths, shard holder names, socket
  addresses or request URLs. The detail goes to the node's own log with the
  job or request it belongs to (#189).
- Personal data is removed from the repository's tree: a retired
  volatile-state capture, local account names in documentation paths,
  scripts and probe examples, and third-party addresses in an archived note
  (#187). Nothing changes at run time.
- A stage holder can verify several rows in one pass (#185), and several
  sequences can be batched exactly (#193). Neither is used by the node yet.
  An execution-mode equivalence suite checks one-pass, chunked,
  restored-prefix and speculative runs against pinned references (#179).
- Off by default: `--community-release-on-verification` lets a coordinator
  answer a verified community prompt before the reward approvals are
  collected (#180), and `--community-verify-profiles` lets a validator also
  verify the interleaved-RoPE profile, defaulting to `legacy-split-half` as
  today (#194). With both off, dispatch, verification and rewards behave as
  in v0.8.11.
- The TypeScript SDK `npm audit` failure in CI is fixed (#172) and a SIGPIPE
  flake in the release contract test is fixed (#184). New CI-only workflows
  run the CPU kernel tests and benchmarks (`kernel-bench`,
  `cpu-matrix-kernels`, and the label-gated `cpu-matrix-bench` and
  `multi-seq-bench`).

## v0.8.11 - Release-preparation snapshot (2026-10-06)

> **Candidate note:** This entry records the v0.8.11 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.11.

- Community workers no longer publish their computer's hostname. A worker
  registers as `node-` plus the first eight hex characters of its address,
  or under a validated `--node-name` nickname that cannot look like an
  operating system's default computer name or another worker's label.
  Validators redact names when a worker registers and again whenever
  `/workers/scoreboard` or `/community/list` is served, so names stored by
  older workers stop being published once validators run this release.
- Windows and Linux machines that report slightly under 16 GB (within 15%,
  or at least 1 GiB, of the marketed size) count as 16 GB for worker
  eligibility.
- Model downloads resume after an interruption, show progress, and are
  checked against the pinned size and SHA-256 before the file is used.
  A mirror that keeps restarting from byte 0 stops the download after a
  bounded number of restarts instead of re-fetching the file indefinitely.
- The macOS install steps follow macOS 15's Open Anyway flow.
- Contributing compute is an explicit choice: a Settings switch, or choosing
  the model during setup, which now says that it means running ARC jobs.
  Once on, the app downloads the model if needed, switches to worker mode
  and restarts the node. Turning it off returns the node to observer mode
  without loading the model, and a refusal saved during a download is
  never overwritten. Ordinary Settings saves never change this choice,
  the role or the model path. Keeping the computer awake during a job is
  a separate opt-in.
- Community workers re-register with a coordinator that forgets them (at
  most once per 15 seconds per coordinator), back off between failed claim
  rounds, and report their job counters to the desktop
  app over loopback (`/community/worker/status`), which shows them on a
  Jobs card.
- Coordinators can dispatch community jobs to two independent workers and
  compare their outputs (twin execution), keep coarse region tags from
  opt-in worker latency reports, and generate demo demand. Every switch is
  off by default. With them off, dispatch, claims, submissions, rewards and
  `/workers/scoreboard` behave as in v0.8.10, and no consensus or on-chain
  rule changes.
- SmolLM3-3B is available as an additional deterministic integer model
  package with its own converter and `arc-modern` tool. The network's model
  and every validator and worker inference path are unchanged.
- The TypeScript SDK tests move to jest 30, and the lockfiles audited for
  release builds no longer contain packages with published advisories:
  braces and sprintf-js leave the TypeScript SDK test tree, desktop moves to
  source-map-js 1.2.2, and the dashboard serves its compiled CSS as a
  reviewed static file with no npm dependencies.
- The Apple Silicon jobs of the required CI checks can run on a self-hosted
  Mac Studio runner once a repository variable is set. Until then, and
  always for fork and Dependabot pull requests and for the release
  workflows, they run on GitHub-hosted runners.

## v0.8.10 - Release-preparation snapshot (2026-09-30)

> **Candidate note:** This entry records the v0.8.10 release-preparation
> snapshot. It does not claim publication or deployment of v0.8.10.

- Shortens local absence grace from 2,000 ms to 250 ms for recently absent
  validators, restoring the full grace on newer blocks without changing
  quorum, certificate validity, or commit rules.
- Validators re-sign a reward commitment they already verified when a
  coordinator retries, instead of recomputing it. Reward approvals converge
  without five concurrent recomputations overloading the 12-slot shard queue;
  the cached approval remains bound to the exact verified commitment.
- Native paid request admission stays open during bounded request execution
  and brief row-relay reconnects (30 seconds), while still closing immediately
  on runtime errors or policy mismatch.
- Clean validator stops exit immediately after the WAL durability barrier,
  and P2P transport shutdown is bounded. This fixes stops lasting 150 seconds
  or more that exceeded the 120-second cutover window, while preserving
  durable state before exit.
- Installers on update-profile releases explicitly refuse legacy v0.7.7
  migration instead of failing later on a missing migration asset.

## v0.8.9 - Release-preparation snapshot (2026-09-29)

> **Candidate note:** v0.8.8 was deployed on all six validators with strict
> post-start agreement and independent finality, and its community dispatch
> fix is live on the fleet. Its Release ARC run (36668823549) stopped in the
> full quality gate before any signing or publication because a new advisory
> against `brace-expansion <=1.1.20` (GHSA-q2hr-2g5m-vwhr, GHSA-qhr7-859c-m2p7,
> GHSA-6j4f-fj2g-mc7p) was published after the v0.8.7 release; the v0.8.8 tag
> stays unpublished. This entry does not claim publication or deployment of
> v0.8.9.

- Fixes community reward verification never completing on the live fleet:
  the fan-out hop returned as soon as two of three replicas agreed and aborted
  the third request, so that replica missed the position, answered the next
  one with `kv_cache_out_of_sync`, and was evicted below the fixed
  three-replica quorum that reward verification requires ("fixed quorum
  unavailable: 2 live replica(s) remain, but 3 are required"). Free fan-out
  still returns at the threshold; a fixed quorum now waits for every selected
  replica so each keeps its KV cache warm. Live v0.8.8 evidence: a published
  v0.8.7 app request was routed to a Linux community worker and answered, but
  every coordinator's verification failed this way, so no 0x25 reward could
  be approved.
- Updates `brace-expansion` 1.1.18 to 1.1.21 in the `sdks/typescript`
  lockfile (a lockfile-only `npm audit fix`), so every JavaScript lockfile the
  release gate audits is clean again.

## v0.8.8 - Release-preparation snapshot (2026-09-29)

> **Candidate note:** v0.8.7 was published and deployed on all six
> validators, and its registration fix works: a community worker now
> registers on every coordinator. Post-release verification of v0.8.7 found
> the defect below; this entry does not claim publication or fleet deployment
> of v0.8.8.

- Fixes community dispatch never reaching an idle worker: workers long-poll
  `/community/claim_work` on every coordinator at once, and each pending poll
  holds an empty reservation in the coordinator's active-job map until work
  arrives. Readiness and routing treated any reservation as a busy worker, so
  a registered, idle worker was counted as zero dispatch capacity on all six
  coordinators (except in the gaps between polls) and requests ran locally
  instead of earning community work. A worker is now busy only while its
  reservation holds an assigned job id, with a regression test for both
  states.
- Published-artifact acceptance: the extracted Windows desktop version check
  now follows the verified MSI ProductVersion instead of a stale `0.8.0`
  pattern, and a failing Linux acceptance job prints and uploads the
  installer evidence it redirects to files, so a failure shows its reason.

## v0.8.7 - Release-preparation snapshot (2026-09-29)

> **Candidate note:** v0.8.6 was published and deployed on all six
> validators. Post-release verification of v0.8.6 found the defects below;
> this entry does not claim publication or fleet deployment of v0.8.7.

- Fixes community-worker registration: coordinators publish
  `validator_address` and `transaction_domain` 0x-prefixed, and the worker's
  signed register/heartbeat path parsed them as bare hex, so every
  registration failed before its POST (logged only at DEBUG). The audience is
  now parsed exactly like the other validator HTTP audiences, with a
  regression test on the live `/network/info` shape.
- Logs redundant gossip deliveries of blocks already in the DAG at DEBUG
  instead of WARN (still counted in consensus diagnostics); at WARN they wrote
  about 2 GB of syslog per validator per day.
- Published-artifact acceptance: installs only `libayatana-appindicator3-1`
  (it conflicts with `libappindicator3-1` on Ubuntu 24.04), and accepts the
  32-bit NSIS installer stub while still requiring the installed
  `arc-desktop.exe` to be AMD64. The AppImage live gate drops the same
  conflicting package, and the desktop runbook accepts the v2 update binding.
- Advances active release and acceptance pins to v0.8.7.

## v0.8.6 - Release-preparation snapshot (2026-09-29)

> **Candidate note:** This source prepares a new release after the immutable
> v0.8.5 release attempt failed while staging its unsigned manifest. This entry
> does not claim publication or fleet deployment; verify those states from the
> exact release and signed rollout evidence.

- Advances active release and acceptance pins to v0.8.6.
- Excludes the exact sibling ownership receipt that `assemble-release.sh`
  writes beside `release-files/` from the manifest-staging source check.
- Runs the real staging cleanliness block against the assembler's outputs in
  the release workflow contract test, so a rejected receipt fails in CI.
- Lets the macOS package provenance controller and the AppImage live gate
  accept the `arc.published-release-binding.v2` binding that
  `published-artifact-acceptance.py bind` emits for existing-chain update
  releases, only with the exact `existing-recovered-chain-update-v1` profile.
  Both previously accepted only the cutover-profile v1 binding.
- Records the v0.8.5 failure (Release ARC run 36569256300): every quality,
  golden-vector and pre-tag verification job passed, then staging rejected
  `.release-files.arc-release-output-owner`. No runtime behavior changes are
  intended.

## v0.8.5 - Release-preparation snapshot (2026-09-29)

> **Candidate note:** This source prepares a new release after the immutable
> v0.8.4 release attempt failed its release/installer contract gate. This entry
> does not claim publication or fleet deployment; verify those states from the
> exact release and signed rollout evidence.

- Advances active release and acceptance pins to v0.8.5.
- Corrects the documented Playwright inventory to 281 tests in 22 files and
  checks it after dependency installation in required desktop CI.
- Records the v0.8.4 failure caused by a stale documented Playwright inventory;
  no runtime behavior changes are intended.

## v0.8.4 - Release-preparation snapshot (2026-09-29)

> **Candidate note:** This source prepares a new release after the immutable
> v0.8.3 attempt failed. This entry does not claim publication or fleet
> deployment; verify those states from the exact release and signed rollout
> evidence.

- Advances the release source, exact-tag acceptance, installer guidance, and
  application metadata to v0.8.4.
- Fixes release fixture profile isolation so cutover assembly uses its intended
  profile when the caller exports another profile.

## v0.8.3 - Release-preparation snapshot (2026-09-28)

> **Candidate note:** This source prepares v0.8.3 after the immutable v0.8.2
> publication gate failed. This entry does not claim publication or fleet
> deployment; verify those states from the exact release and signed rollout
> evidence.

- Retargets the current release manifests, installer guidance, and acceptance
  gates to v0.8.3 while retaining the historical v0.8.2 entry below.
- Fixes GPU staging-buffer usage when mappable primary buffers are unavailable
  and adds a staged upload/readback regression.
- Retains failed quality-gate logs as diagnostics so release failures identify
  their originating check.

## v0.8.2 - Release-preparation snapshot (2026-09-28)

> **Publication retry note:** The protected v0.8.1 tag is immutable, but its
> publication workflow failed before assets were uploaded. This source targets
> v0.8.2; this entry does not claim that release assets are published or that
> the fleet has been upgraded. Verify those states only from exact release and
> signed rollout evidence.

- Retargets application manifests, release acceptance, and install guidance to
  the new exact v0.8.2 tag while preserving v0.8.1 and v0.8.0 records below.
- Fixes nested health-regex shell quoting and adds a focused workflow smoke
  regression before attempting publication under the new immutable tag.

## v0.8.1 - Release-preparation snapshot (2026-09-28)

> **Release-preparation note:** This source snapshot prepares v0.8.1; it does
> not claim that the tag has been published or that the fleet has been
> upgraded. Verify those states only from the exact immutable release and
> signed rollout evidence.

- Retargets published-artifact acceptance and installer checks to the exact
  v0.8.1 release while preserving the older v0.8.0 source-freeze record.
- Improves the desktop earnings view and explorer copy so retained historical
  receipts are shown only when available and a signed checkpoint is not treated
  as proof that older transaction bodies were retained.
- Keeps the Linux packaged-node acceptance check bound to the selected release
  version and rejects version-prefix matches.

## v0.8.0 - Release-preparation snapshot (2026-08-31)

> **Tag-stable lifecycle note:** At this review cutoff, v0.8.0 was an
> unreleased recovery candidate and was not deployed or published. An
> immutable v0.8.0 tag intentionally retains that historical fact. Determine
> current publication and fleet status from the exact immutable release API
> evidence and the signed coordinated-rollout receipts, not from this changelog
> snapshot.

- Restores one checksummed release graph for headless Linux amd64/arm64,
  Intel/Apple-Silicon macOS, Windows CLI, and signed desktop bundles. The
  installer resolves one exact version, works without a display, and refuses
  downgrade or unverified replacement. Workspace, Tauri, desktop npm, and lock
  metadata are all pinned to v0.8.0 before a matching tag can publish.
- Splits unsigned builds, updater-payload signing, release-manifest signing,
  draft publication, and read-only server verification across exact-ID,
  digest-bound handoffs. Fresh protected signers run no repository program and
  expose keys only to per-boundary absolute executable allowlists; the Node
  signer runtime is exact-patch and byte bound. Publication never deletes a
  release after its publish PATCH is attempted and boundedly polls GitHub's
  eventually consistent immutable state before sealing evidence.
- Adds the typed `arc health` command and makes the installer use it for
  readiness, accepting only explicit JSON `ok` or `degraded` states. Install
  and upgrade now share a complete rollback transaction across binaries,
  configuration, identity, service definitions, and prior service state.
- Adds a blocking release/security harness: Rust format/check/Clippy/test
  coverage, GUI-free node boot gates for Linux x86_64 on Ubuntu
  22.04/24.04/26.04 and ARM64 on Ubuntu 24.04/26.04, deterministic desktop E2E,
  Tauri tests on every released desktop architecture, SDK packed-consumer
  tests, workflow/ShellCheck contracts, and staged plus working-copy secret
  scans.
- Replaces same-host determinism claims with hardcoded production-engine KATs
  covering CPU I8/I16, 1-versus-4 threads, whole-versus-three-way sharding,
  logits/KV/hidden-state hashes, and autoregressive output across ARM and x86.
- Makes sequential and BlockSTM execution use the same canonical ordering and
  state semantics; consensus timestamps and DAG attachments are verified and
  peer hints can no longer mutate authoritative state.
- Binds every persistent database to its authenticated genesis network hash,
  rejects unmarked legacy WAL reuse, requires production validators to exist
  in the canonical genesis accounts, binds native RPC to loopback by default,
  and leaves Ethereum RPC disabled unless an operator explicitly enables it.
- Replaces the abandoned bincode 1.x implementation with a bounded internal
  v1-wire-compatible facade, and narrowly patches Wasmer's derive crate to
  remove `proc-macro-error2` while preserving upstream provenance and valid
  generated code. Remaining upstream advisories stay blocking in the release
  gate; none are ignored.
- Introduces authenticated v3 community registration/heartbeat/claim/submit,
  exact model-ID routing, one-job worker capacity, bounded payloads/timeouts,
  independent 2-of-3 range verification, and five-of-six active-validator,
  replay-protected 2.5 ARC reward transactions. Stable policy, approval, job,
  receipt, and earnings endpoints bind evidence to the recovery epoch,
  validator set, transaction domain, exact model/input/output, and mined
  `0x25` receipt. Pending or rejected rewards are never reported as earned;
  projections fail closed unless policy, receipt history, and treasury support
  them. Stake-zero worker eligibility is explicit policy, not an installer
  promise.
- Prevents automatic inference fallback from replaying one UI click across
  local, direct-coordinator, and consensus write routes. Clients may probe the
  exact read-only `/inference/readiness` contract in parallel, but select one
  origin and issue at most one inference POST. Every timeout, connection reset,
  HTTP error, or malformed response after that POST is treated as an ambiguous
  possibly-settled outcome and is never retried on another coordinator.
- Separates RPC from P2P discovery and configures all six reviewed literal-IPv4
  HTTPS origins explicitly. The locked SHA-pinned Caddy 2.11.4 gateway requests
  publicly trusted Let's Encrypt IP certificates with the `shortlived` profile
  over HTTP-01, removing the shared `nip.io`/`sslip.io` wildcard-DNS dependency.
  Remote plaintext, credentials, URL paths, query strings, fragments, wildcard
  listeners, and port zero are rejected outside the deliberate local/dev escape
  hatch.
- Moves wallet transfer signing into Rust so the seed never crosses IPC,
  parses and formats ARC with exactly nine decimal base-unit precision, and
  treats sends and 1 ARC faucet claims as pending until the chain returns a
  successful mined receipt.
- Authenticates P2P session transcripts with TLS-exporter-bound Ed25519
  identities, enforces pinned certificates in strict mode, caps frames and
  decoders, and binds claimed identities to signed payloads.
- Hardens validator identity and rollout: validator key files are mandatory,
  legacy exposed keys are rejected, genesis/release contracts fail closed, and
  the six-node v3 cutover is explicitly coordinated rather than auto-deployed.
- Adds an archive-bound recovery transaction around that cutover: a sealed
  freeze plan, two independently captured quarantine samples at least 120
  seconds apart, per-node stopped-writer/listener evidence, content-indexed
  legacy bundles, and separately verified Google Drive completion roots must
  all cross-bind before validator mutation. Every divergent legacy lineage is
  retained with an explicit canonical, non-canonical-fork, or unclassified
  disposition; recovery never rewrites those forks into one invented history.
- Adds a boundary/tool/source-set-bound late-fork interlock. All six recovered
  gateways must publish a fresh healthy status. A coherent legacy observation
  above the sealed public-height cutoff creates a persistent incident and
  forces the dashboard and explorer back to maintenance; it never auto-clears
  or promotes the observation into canonical history.
- Adds one-shot validator-vault rewrap and restore/install tooling. The
  passphrase-encrypted source vault is metadata-validated and re-encrypted to
  an operator-supplied CMS certificate without publishing plaintext. Restore
  is exact-main/pre-tag/profile bound, and remote key installation is
  create-only over pinned SSH only after authenticated offline-stop evidence
  v2 proves the legacy writers remain fenced.
- Makes community reward activation an authenticated genesis schedule plus an
  independent local issuance switch. An absent activation height disables tx
  `0x25`, and the issuance flag cannot override that absence. The checked-in,
  checkpoint-bound recovery genesis schedules activation at block `137146`;
  rollout readiness and the independent local switch still keep issuance
  fail-closed until the approved coordinated cutover.
- Reworks dashboard and explorer rendering for safe DOM insertion, honest
  liveness/retained-history semantics, coordinator-specific compatible worker
  capacity, visible inference/quorum/settlement evidence, actual on-chain
  balances, and a fail-closed six-replica maintenance interlock. Production
  dashboard CSS is compiled locally instead of executing the Tailwind
  development CDN.

At the 2026-08-31 source freeze, this candidate was not deployed or published,
and the public fleet remained split across old v0.7.2/v0.7.9 binaries. A valid
current deployment claim requires operators to have rotated compromised keys,
chosen a clean-genesis or checkpoint recovery policy, and completed the signed
rollout gate.

## v0.7.11 - 2026-06-15 (desktop-only)

- Removed the On-chain Tier 1 radio and the tier1 mutation, polling, and
  status panel from the desktop UI (`Inference.tsx` -348 lines,
  `Settings.tsx` -55). The feature was withdrawn rather than fixed: a
  caller-signed `InferenceRequest` is accepted with HTTP 200 by
  `/inference/onchain/submit` but never lands in a block, so escrow never
  opens and the committee never votes. See
  `docs/INFERENCE_TIER1_INVESTIGATION_2026-06-04.md`. (57ff20d)
- Desktop default `max_tokens` halved 32 → 16, noted as "~3 min vs ~6 min per
  request on the public testnet" — i.e. ~11 s/token. (d60632a)

## v0.7.10 - 2026-06-11 (desktop-only)

- Aligned the desktop with live chain state. Deliberately released without a
  tag push so `release.yml` would **not** publish `arc-node` binaries from a
  main that lacked the wire-compat fixes the seeds were running. This is the
  change that broke `install-community-node.sh`, which resolved
  `releases/latest` and then 404'd on a CLI asset that was no longer there.
  (281bdd0)
- Hid the on-chain inference submit path (tier-1 + paid escrow) in the desktop
  and coerced persisted `"onchain"` mode back to `"coordinator"`. (306ebb7)
- Documented the tier-1 inclusion failure. (fe1ed8d)
- Wrote the replicated-chain (Model 1, full re-execution) implementation plan.
  Not started as of 2026-08-17. (99cf84f)
- Investigated and documented that the testnet seeds are **independent
  chains**, not one network. (95a9686)

## v0.7.8 / v0.7.9 - 2026-06-01 → 2026-06-04 (no tag, no release)

The binary the five non-NYC seeds actually run. Built from
`fix/v078-attestation-wire-compat`; merged to main 2026-06-16 (f6bee03).

- `InferenceAttestation` made wire-compatible with v0.7.2 validators — the
  reason NYC (still v0.7.2) can interoperate at all. Wire size pinned by a
  unit test. (24150b3, 5279838)
- `--community`: one-flag setup with auto-model-discovery, and auto-download
  of a sha-pinned Llama-2-7B GGUF. (dcab105, 397698e)
- Validator auto-shard via `/shards/join`, plus a canonical `model_id`
  function. (ff46e53) — note this is the path that can wedge a fully-covered
  public pipeline; see the safety rules in `CLAUDE.md`.
- Scalable model distribution: multi-source fetch, LFS pre-check, resume.
  (601194c)
- Sharded inference unblocked; smart-router timeout cut. (30b3113)
- INT16 enabled on shard-holders — `enable_i16()` now called after load so the
  I16 dispatch path is actually taken. (0d9d6a4, 613a232)
- `tier1_pending` restored across restarts; partial-local requests routed to
  the sharded path. (0d9d6a4)
- Context window doubled: RoPE 2048 → 4096, sharded output cap 256 → 1024.
  (96b87fe)
- `/inference/onchain/submit` accepts a caller-signed tx and reports
  diagnostics; inference-validator retries finalize on a stuck request. Neither
  made the transaction land in a block. (34e1fd0, 631e5b0)
- `rolling-upgrade.sh` pauses `arc-self-heal` per node and re-arms it after the
  health gate. (9dc7618)

## v0.7.7 - 2026-05-29

- Desktop Tier 1 routes to the public testnet seeds; the alpha VPS is retired.
  (37df67b)
- **Last release to ship `arc-node` CLI binaries** (linux-x86_64,
  macos-arm64, macos-x86_64, windows-x86_64.exe).

## v0.7.6 - 2026-05-29

Ten commits of wallet-host churn, which is worth reading as one story: the
desktop wallet was pointed at alpha, then at 5 seeds, then at LAX, then back to
localhost, then at the live testnet. It settled on pinning a single seed —
`docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md` later explained why aggregation
across seeds could never work. (800d828, 6226342, b05bed5, 8e151de, bf1e14c,
4876820, f040d23, 671cf03)

- "Option C": beneficiary credit for tier-1 `InferenceAttestation`. The
  `beneficiary` wire field was subsequently marked `#[serde(skip)]` after it
  partitioned the validator set on 2026-05-29. (9d75170)
- arc-state: speculative `success=false` routed to unresolved — a BlockSTM fix
  for tier 1. (4cff9ea)

## v0.7.5 - 2026-05-28

- arc-node posts an `InferenceAttestation` after every Tier 1 vote. (28e18dc)
- Desktop wallet (balance / faucet / earnings) routed to alpha rather than
  127.0.0.1. (671cf03)

## v0.7.4 - 2026-05-28

- Desktop actually invokes the Tauri auto-update and relaunches after install —
  previously the update was downloaded but never applied. (b3014b9)
- README download links bumped v0.6.0 → v0.7.3. (3218d8f)

## v0.7.3 - 2026-05-26

- Tier 1 on-chain inference gains load balancing and error handling. (9cbbaab)
- Desktop tier1 pinned to the alpha solo node (v0.7.2) while the
  multi-validator inclusion bug stayed open. (295ee6f)

## v0.7.2 - 2026-05-20

- **Tier 1 on-chain inference, Phase A.** (73eaee9)
- Inference timeout raised 3 s → 120 s, and a false `503 Pipeline gap` fixed.
  The 3-second timeout had made sharded inference unusable. (4acbcbb)
- Connecting UX: syncing banner + onboarding progress. (0510fa4)
- Alpha VPS hardening and UX polish. (34a393f, 9308d9d)
- At the 2026-08-31 source freeze, this was the version **NYC still ran**.

## v0.7.1 - 2026-05-12

- **Validator-signed `FaucetClaim` TxType**, gated behind `FAUCET_V2_ENABLED`
  and then defaulted on. This is why faucet credits propagate while
  coordinator-minted attestations do not. (e0ad962, 548d3c3)
- Signer-nonce check dropped from the `FaucetClaim` executor. (564468c)
- `StateDB.validators` seeded from genesis at startup, and
  `seed_genesis_validators` clears the set first. (4771d35, 40bab32)

## v0.7.0 - "Just be a node, and earn for real"

The community-worker system actually works for the first time. Pre-v0.7
the seed coordinators advertised a work queue that was never wired up:
NodeState's `community_work_tx`, `_queue`, and `_results` were
declared but always set to `None`, so every `/community/claim_work`
poll returned 503 "work queue not initialized." Workers polled,
heartbeated, and ran the literal definition of doing nothing — which
matches every "12 peers connected, 0 attestations, 0 earnings forever"
report from v0.6 users.

### Architecture
- `rpc::serve()` now wires a real bounded mpsc (256 slots) for
  community work. `claim_work` long-poll, `submit_work` round-trip,
  and the seed-side dispatcher all share that channel.
- `WorkItem` and `WorkResult` redesigned around whole-prompt routing
  (`job_id`, `input`, `max_tokens` → `output`, `output_hash`,
  `tokens_generated`). The half-built layer-shard community-work
  shape is gone; layer-shard remains a seed-to-seed primitive
  (`forward_shard`).
- `/inference/run` is now a smart router: prefers a community worker
  when any are online, falls back to the seed's local model otherwise.
  EWMA latency is recorded per worker for future scoring (task 4).
- Workers sign `InferenceAttestation` (tx 0x16) with their validator
  key and submit alongside results; the seed verifies and inserts
  to mempool. Earnings are now actual on-chain credits.
- `GET /worker/earnings/:address` reads chain state directly. The
  desktop's pre-v0.7 client-side synthesis from
  `/inference/results` (which conflated "this seed's local cache"
  with "this address's network earnings") is gone.

### Desktop
- Lite-mode banner rewritten as honest "Client mode": tells users
  they're connected as a client, will not earn ARC until peers > 0,
  and offers a "Reset peer state & rebootstrap" button that wipes
  `<data_dir>/known_peers.json` and restarts. The most common cause
  of "I had peers, then I restarted, now stuck" is a stale dial cache,
  and this fixes it in one click.
- New Tauri command: `reset_peer_state`.

### Removed
- **`scripts/community-gateway.py`** — Python sidecar from v0.5.2
  on port 3001. All functionality is now in arc-node on port 9090.
  The legacy port 3001 fallback in the worker loop and registration
  script is kept temporarily for the rolling-upgrade window; remove
  in v0.7.x once all seeds run v0.7.0+.

## Unreleased - 2026-04-22

**Cluster self-heal, latency-aware routing, slashing hook-in, retirement of dead seeds.**

### Self-heal daemon (GH #30)
- **`scripts/arc-self-heal.sh`** - on-host bash daemon running as a systemd
  unit on each seed. Polls `http://127.0.0.1:9090/health` every 30 s and
  restarts arc-node on either (a) RPC silence ≥180 s or (b) `dag_round`
  unchanged ≥300 s while a remote peer is ≥100 rounds ahead. Captures
  every `--shard-range`, `--model`, and `ARC_PUBLIC_SOCKET` from
  `/proc/PID/cmdline` + `/proc/PID/environ` so restarts reuse the live
  argv exactly. Persists last-good snapshot to
  `/root/arc-chain/.self-heal-last-good.sh` so the daemon can relaunch
  arc-node from scratch if the process is already dead when the silent
  threshold fires.
- **`scripts/arc-self-heal.service`** - systemd unit with `KillMode=process`
  (so `systemctl restart arc-self-heal` doesn't take arc-node down as
  cgroup collateral) and NO `MemoryMax` (arc-node inherits the cgroup
  via setsid and a cap on the supervisor OOM-kills the node during the
  1 GB embedding load).
- **`scripts/install-self-heal.sh`** - idempotent per-host installer;
  refuses to install if `systemctl is-active arc-node` returns active
  (would conflict with the daemon's spawn path).
- 5 min `RESTART_DEBOUNCE` (bumped from initial 300 s proposal to 600 s)
  so a cold-boot cycle fits inside one debounce without flapping.
- `MIN_HEALTHY_PEERS=4` safety rail - drift-triggered restart refuses to
  fire if fewer than 4 remote peers are healthy, preventing a cascade
  from taking consensus below quorum.
- Deployed to all 6 seeds; two live repairs captured in-session.

### Coordinator routing (GH #29)
- **`NodeState.latency_stats`** - `DashMap<socket_addr, LatencyEWMA>`
  (α=0.2). Folded on every successful `forward_shard` hop in both
  `inference_run_sharded` (prefill worker + gen loop) and
  `inference_run_consensus` (per-replica parallel dispatch).
- **Per-range replica lists sorted by EWMA ascending** before the
  coordinator picks primary (`run_sharded`) or top-k (`run_consensus`).
  Unseen replicas keep insertion order at the tail so cold-start doesn't
  starve first-try dispatch.
- **`GET /inference/latency_stats`** - exposes the map sorted ascending
  for dashboard / diagnostics. Does not affect determinism - only which
  replicas answer, not what they answer.

### Divergence → on-chain slashing (GH #31)
- When `/inference/run_consensus` records non-empty `divergent_replicas`,
  the handler auto-submits one `InferenceCommitment` per divergent
  replica via `arc_vm::inference_verify::VerificationManager` and one
  `VerificationChallenge(ConsensusVerification)` from the coordinator.
- Response JSON gains `consensus.auto_challenges[]` with
  `commitment_id`, `challenge_id`, `divergent_replica`, `their_hash`.
- `AUTO_CHALLENGE_BOND = 100_000` is a placeholder; final value + payer
  (coordinator treasury vs honest-majority split) pending operator call.
- Divergent-replica provider identity derived as
  `hash("divergent:<node_name>")` - stable pseudo-ID until real
  validator-address reconciliation lands.

### Cosmetic (GH #33)
- **`/shards fully_covered`** - walks `BTreeSet<(start_layer, end_layer)>`
  to dedup the 18-replica list before the contiguity check. Returns
  `fully_covered=true` on a healthy 3×-replicated deployment.

### Dashboard (GH #34)
- **`dashboard/index.html`** - all 6 inference fetch sites swapped from
  `/inference/run_sharded` to `/inference/run_consensus` with `k:3`.
  The "all passing consensus" story now shows in the UI by default.

### Retirements (GH #32)
- **SAO (216.238.120.27)** and **JNB (139.84.237.49)** retired from the
  validator set: removed from `testnet-seeds.txt` (pushed to all 6 live
  seeds) and from `genesis.toml` (local only - live DAG history still
  has the 8-validator set; change takes effect on next coordinated
  genesis event). Both had been RPC-dead for weeks with unreliable
  datacenter connectivity; 6-seed × 3× replication already covers the
  inference pipeline.

## v0.5.2 - 2026-04-11

**Community inference network + SIMD performance + audit hardening.**

### Community Worker Infrastructure
- **Community gateway sidecar (REMOVED IN v0.7.0)** (`scripts/community-gateway.py`) - Python
  HTTP server on port 3001 that runs alongside arc-node. Handles worker
  registration, heartbeat, inference job distribution via long-poll.
- **`POST /inference/community`** - submit inference to be computed by
  any available community worker. Gateway queues the request, worker
  claims via `/community/claim_work`, computes locally, submits result
  via `/community/submit_work`. All outbound-HTTPS, works behind NAT.
- **`--community-mode`** CLI flag - arc-node auto-registers with all
  seed gateways (port 3001), heartbeats every 15s, polls for inference
  jobs in background. Works with any model loaded.
- **Standalone registration script** (`scripts/arc-community-register.sh`)
  - bash loop that registers ANY version of arc-node with gateways.
  No binary rebuild needed. `curl ... | bash` one-liner.
- **UDP 443 fallback** on all 8 seeds via iptables redirect. Community
  nodes behind ISPs that block UDP 9091 can connect via 443.
- **`scripts/arc-diagnose.sh`** - 4-phase health check for community
  nodes: UDP reachability, process status, peer count, chain sync.
- Install script now checks peer count and warns if node is isolated.

### Performance (NEON SIMD on Mac M2 Ultra)
- **NEON i16 dot product** (`dot_i16_i64_neon`) - 3.7× matmul speedup.
  Vectorized 8-lane i16×i32→i64 via vmlal_s32.
- **NEON attention Q·K SIMD** (`dot_i64xi64_attn_neon`) - vectorized
  attention inner loop. Heads parallelized via `into_par_iter()`.
- **Rayon chunk-256** - empirical sweet spot for M2 Ultra core
  saturation. Combined Mac compute: **23× faster** (14.4s → 622ms
  per position).
- **Q4 NEON wiring** - opt-in via `ARC_Q4_SHARD=1`. Dispatch prefers
  Q4→I16→I8 on aarch64. Disabled by default (precision risk on 7B).
- AVX-512 i16 widening attempted but reverted (consensus segfault on
  Vultr Skylake Xeon). x86 stays on scalar fallback.

### Stability & Correctness (11 audit fixes)
- `debug_assert` bounds checks in all 8 matmul functions.
- GPU `recv().unwrap()` replaced with graceful error handling.
- 32KB prompt length limit added to `inference_run_sharded`.
- Faucet rate limit documented as testnet-only.
- Gateway default port fixed (9090 → 9944 to match binary).
- New tests: `test_q4_scale_roundtrip`, `test_i16_matmul_nonzero_output`.
- Shard registry 60s TTL + self-refresh broadcaster.
- Puller only pulls `self_shard` from each seed (prevents stale entry
  resurrection).
- No-candle on shard holders saves ~4 GB RAM on 8 GB VPS.
- `forward_one_token` panic guard for non-first shards.
- Pipelined prefill + chat template skip (~4× faster first-token).

### Infrastructure
- Shard dedup fix: pipeline walker prefers routable socket_addr.
- Updated `testnet-seeds.txt` with `:443` fallback entries (git only;
  seeds themselves use `:9091` to avoid self-dial).
- Community gateway deployed as systemd service on 6 seeds.

### Tests
- 68 tests pass (was 64 in v0.5.1). New: dot_i16 SIMD correctness,
  Q4 scale roundtrip, I16 matmul nonzero, dot_i16 tail.

## v0.5.1 - 2026-04-08

**Dashboard cache warmth indicator.** The dashboard now probes the
coordinator's `DistributedCache` on page load and tags preset prompt
buttons that are already warm with a green `⚡ INSTANT` badge. Visitors
know at a glance which clicks will return in ~100 ms (cache hit,
~200 tok/s effective) vs which will run the full 7-shard pipeline.

New RPC endpoints:
- `GET /inference/cache_stats` - `{size, capacity, total_hits,
  cache_type}`. Dashboards call this to show "N entries cached ·
  K cumulative hits" under the preset prompt row.
- `POST /inference/cache_check` - body: `{prompts: [{input, max_tokens},
  ...]}`, returns `[{input, max_tokens, cached: bool}, ...]`. Tokenizes
  server-side and re-derives the BLAKE3 cache key the exact same way
  `inference_run_sharded` does, so a check here matches what a real
  call would look up. Does NOT bump `hit_count` - a warmth probe
  should not distort LRU ordering.

`DistributedCache` gains `contains()`, `capacity()`, and `total_hits()`
helpers. `contains()` is a non-mutating probe; the existing `get()`
still bumps `hit_count` for LRU ordering.

Measured tokens/sec (live testnet, 2026-04-08):
- Cache HIT (repeat): ~200 tok/s wall (94-109 ms for 20 tokens);
  server-side serve time is 17-37 μs, the rest is HTTP roundtrip.
- Cache MISS (novel): ~0.04 tok/s (~22 sec/token through full
  7-shard pipeline).
- Speedup ratio: ~5,000× for cached results.

## v0.5.0 - 2026-04-08

**Deterministic inference cache wired into sharded handler.** The
`DistributedCache` in `crates/arc-inference/src/distributed.rs` was
fully implemented but never connected to any RPC handler. Now wired
into `inference_run_sharded`:

- **First call** with a given `(model_id, input_tokens, max_tokens)`
  triple runs the full 7-shard pipeline (~12-15 sec/token), inserts
  the result into the cache.
- **Every subsequent call** with the same triple returns in **O(1)**
  (~10-50 microseconds) with `cache.hit: true` and the same
  `output_hash`. Provably bit-identical because integer determinism
  guarantees same input → same output.
- 10,000-entry cache with LRU eviction by hit count.
- Response includes a `cache: { hit, key, size, served_in_us }` block
  so the dashboard can render a CACHE HIT badge.

This is the first user-visible 10× speedup. The dashboard's preset
prompt buttons (which always send the same prompts) now feel instant
after the first click. The 7-shard pipeline is still slow for novel
prompts - that's the next optimization (pipeline microbatching).

The cryptographic correctness of cache hits comes from the integer
engine: same model + same input ALWAYS produces the same output.
The cache isn't an approximation, it's a proof.

## v0.4.6 - 2026-04-08

**Apply unique-nonce fix to single-node `/inference/run` too.** v0.4.5 fixed
the duplicate-tx_hash issue for sharded inference but the same bug existed
in the single-node handler. Both endpoints now use the shared
`attestation_nonce: AtomicU64` so any inference call - sharded or not -
produces a unique attestation tx_hash even on repeat prompts.

## v0.4.5 - 2026-04-08

**Unique attestation tx_hash for repeat prompts.** Identical inputs to
`/inference/run_sharded` were producing identical attestation tx_hashes
(same nonce + same body), and the mempool was de-duping the second
submission. Now uses a monotonic `attestation_nonce: AtomicU64` on
NodeState that bumps on every sharded run, so each submission gets a
unique nonce → unique tx_hash → committed independently.

Also: `/tx/{hash}` now accepts both `0x`-prefixed and bare 64-hex forms.

## v0.4.4 - 2026-04-08

**Sharded runs now produce on-chain attestations.** Every successful
`/inference/run_sharded` call now submits an `InferenceAttestation`
transaction to the mempool, just like single-node `/inference/run` does.
The attestation includes `model_id`, `input_hash`, and `output_hash` so
anyone reading the chain can later verify a sharded run actually happened
and produced a specific output. Returns the attestation `tx_hash` and
`explorer_url` in the response.

## v0.4.3 - 2026-04-08

**CI fix.** Dropped `x86_64-apple-darwin` from the release workflow matrix
because GitHub-hosted `macos-13` runners were removed. Releases now ship
`arc-node-macos-arm64` + `arc-node-linux-x86_64`. Intel Mac users can build
from source.

## v0.4.2 - 2026-04-08

**Server-side sharded inference activity counters.** `NodeState` gains
`sharded_runs_total: AtomicU64` and `sharded_bytes_total: AtomicU64`. The
`/inference/run_sharded` handler increments both before responding. The
`/stats` endpoint exposes them as new fields. The dashboard hero shows them
as live "Runs Served" + "Bytes Forwarded" stats and aggregates them across
all 8 seed nodes (resilient to single-node restarts).

## v0.4.1 - 2026-04-08

**Sharded inference INT16 quality fix.** The shard loader was producing
INT8-only weights, which made the integer engine too noisy on Llama-7B -
every prompt collapsed to the same output tokens regardless of input.

Fix: each shard now loads weights as BOTH I8 (kept for fallback) AND I16,
where I16 is quantized directly from f32 with 258× finer granularity (32,767
levels per row vs 127). `forward_shard_token` dispatches to the I16 path
when available. The output_weight (LM head) gets the same treatment so the
final argmax sees better-quantized logits.

After this fix the network produces coherent answers like `"The largest
planet is Jupiter, which is more than 1,31..."` and `"The capital of France
is Paris."`.

## v0.4.0 - 2026-04-08

**Pipeline-parallel sharded inference shipped.** A model is now split across
N nodes at transformer layer boundaries. Each node holds a contiguous slice
of layers and forwards activations to the next shard via HTTP. BLAKE3
verifies every hidden state in transit. The first shard embeds tokens, the
last shard runs the LM head + argmax.

New endpoints:
- `POST /inference/run_sharded` - coordinator endpoint that walks the pipeline
- `POST /inference/forward_shard` - per-shard handler
- `GET /shards` - local shard registry
- `POST /shards/announce` - peers register their shards here

New CLI flags: `--shard-start <usize>` `--shard-end <usize>`. Together they
turn a node into a shard holder for the specified layer range. Without them
the node loads the full model normally.

New crate APIs:
- `arc_inference::cached_integer_model::load_cached_model_shard(path, start, end)`
- `arc_inference::cached_integer_model::CachedIntegerModel::forward_shard_token(input, cache, start, end, position)`
- `arc_inference::cached_integer_model::ShardInput::{Token, Hidden}` and `ShardOutput::{Hidden, Token}`

New scripts:
- `scripts/install-community-node.sh` - one-command community node installer with launchd / systemd persistent service + daily auto-update
- `scripts/arc-demo.sh` - end-to-end demo runner (discover pipeline → run inference → verify determinism → verify isolation)
- `scripts/arc-watchdog.sh` - testnet watchdog that preserves shard flags on restart
- `scripts/arc-health-check.sh` - network-wide health probe

New docs:
- `docs/HOW-SHARDING-WORKS.md` - engineer-grade deep dive
- `docs/SERO-DEMO.md` - 5-minute demo walkthrough
- `docs/ANNOUNCEMENT.md` - shareable summary

Dashboard: new "Sharded AI" hero section with live pipeline diagram, custom
prompt input with preset buttons, per-hop trace replay using actual wall_ms
timings, persisted run history, server-side activity counters, model_id
verification badge, and a copy-pasteable join command.

## v0.3.1 - 2026-04-07

P2P deadlock fix (try_send + broadcast write timeout), Quinn Connection
lifetime fix, commit-scan liveness fallback, observer-mode panic fix,
cross-device attestation reading from chain state, sero-quickstart.sh.

## v0.3.0 - 2026-03-29

Distributed inference parallel mode + consensus partition healing.

## v0.2.x and earlier

See git history.

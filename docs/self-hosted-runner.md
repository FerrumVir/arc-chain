# Self-hosted Mac Studio CI runner

ARC can run its Apple Silicon macOS CI jobs on its own Mac Studio instead of
GitHub's `macos-15` machines. There are two reasons:

- **Queue time.** GitHub runs at most 5 macOS jobs at once for this account.
  On 6 October 2026, 14 macOS jobs were waiting behind them.
- **A real GPU.** GitHub's macOS VMs expose only a paravirtual GPU. The Mac
  Studio's Apple GPU lets the GPU kernels be tested on Metal hardware.

The switch is one repository variable. While it is unset, every job runs on
GitHub-hosted machines exactly as before.

## What runs where

| Workflow | Job (status check name) | Required for `main` | Where it runs |
|---|---|---|---|
| CI | `test (macos-15)` | yes | Mac Studio when the switch is on, else GitHub `macos-15` |
| CI | `desktop Tauri Rust tests (macos-15)` | yes | Mac Studio when the switch is on, else GitHub `macos-15` |
| Golden vectors | `golden vectors (macos-15)` | yes | Mac Studio when the switch is on, else GitHub `macos-15` |
| Linux candidate artifacts | `Desktop native paid Rust tests (macOS)` | no | Mac Studio when the switch is on, else GitHub `macos-15` |
| Metal GPU (self-hosted) | `Metal GPU kernels (arc-mac-studio)` | no | Mac Studio only, opt-in (see below) |
| CI, Golden vectors | every `(macos-15-intel)` leg | yes | Always GitHub (Intel) |
| Release ARC, Release signing preflight, Published artifact acceptance | every macOS leg | n/a | Always GitHub (see "Security model") |

Routing never changes a job's name or matrix values, so the status checks
required by the `main` ruleset keep their exact names.

## How routing works

Each routed job computes `runs-on` from the repository variable
`ARC_MACOS_ARM_RUNNER`:

```yaml
runs-on: ${{ matrix.os == 'macos-15' && startsWith(vars.ARC_MACOS_ARM_RUNNER, '[') && (github.event_name != 'pull_request' || (github.event.pull_request.head.repo.full_name == github.repository && github.event.pull_request.user.login != 'dependabot[bot]')) && fromJSON(vars.ARC_MACOS_ARM_RUNNER) || matrix.os }}
```

A job goes to the Mac Studio only when all three hold:

1. it is the Apple Silicon leg (`macos-15`; Intel legs never move);
2. the variable holds a JSON array of runner labels (it starts with `[`);
3. the run is not a pull request from a fork or from Dependabot. Pushes,
   schedules, manual runs and pull requests whose branch is in this
   repository qualify, except Dependabot's, whose new dependency versions'
   install and build scripts stay on GitHub-hosted machines.

Otherwise the job runs on `macos-15` as before. A value that does not start
with `[` is ignored rather than breaking CI. A malformed array stops the routed
jobs from starting, so copy the value below exactly. The job in
`linux-candidate.yml` has no matrix, so it uses the same expression without
the first condition.

The value to set, once the runner shows **Idle**:

```
["self-hosted","macOS","ARM64","arc-mac-studio"]
```

## Turning it on

1. Check that GitHub → arc-chain → Settings → Actions → Runners lists
   `arc-mac-studio` as **Idle**.
2. Smoke-test the machine without touching required checks: Actions →
   **Metal GPU (self-hosted)** → Run workflow on `main`. This installs the Rust
   toolchain for the runner account (the first run is slow) and shows that the
   GPU is visible from the runner service.
3. Set the variable:
   ```sh
   gh variable set ARC_MACOS_ARM_RUNNER --repo FerrumVir/arc-chain \
     --body '["self-hosted","macOS","ARM64","arc-mac-studio"]'
   ```
4. Push to a branch of this repository (or re-run CI) and confirm that the
   Apple Silicon jobs ran on the Studio and passed:
   ```sh
   gh api repos/FerrumVir/arc-chain/actions/runs/<run-id>/jobs \
     --jq '.jobs[] | select(.name | test("macos")) | {name, runner_name, conclusion}'
   ```
   Routed jobs show `runner_name: arc-mac-studio`; Intel jobs still show a
   `GitHub Actions` runner.

## Turning it off

```sh
gh variable delete ARC_MACOS_ARM_RUNNER --repo FerrumVir/arc-chain
```

New runs go straight back to GitHub-hosted machines. Jobs that were already
queued for the Studio keep waiting for it, so cancel those runs and start fresh
ones (push a commit, or re-run and check the runner name as above).

Turn it off whenever the Studio is offline, asleep or being updated. GitHub has
no automatic fallback: a routed job waits for the Studio for up to 24 hours,
and its required check stays pending the whole time.

To retire the runner completely, also stop its service on the Studio and remove
it under Settings → Actions → Runners.

## Security model

This repository is public, and a self-hosted runner executes the code of every
job it accepts. The design keeps outside code off the machine and keeps
secrets away from it.

**Fork pull requests never run on the Studio.**

- Every routed `runs-on` contains the same-repository condition above. The
  Metal job's `if:` requires a branch in this repository that Dependabot did
  not open, or a manual run.
- The repository requires approval before workflows run for pull requests from
  any outside contributor (`approval_policy: all_external_contributors`).
- For `pull_request` events, GitHub runs the workflow files from the pull
  request itself. A fork could therefore edit a workflow to target the Studio
  directly, so the approval step is the real gate. **Never approve workflows
  for a fork pull request that changes anything under `.github/` without
  reading that change.** The runner-side guard below removes this reliance on
  people.
- Do not add `pull_request_target` or `workflow_run` triggers to a routed
  workflow. Its same-repository condition is written for `pull_request`.

**Who can run code on the Studio:** everyone with write access to this
repository (any branch they push) and anyone who can start workflows manually.
Dependabot pull requests are branches in this repository, but the routed
conditions exclude them (`github.event.pull_request.user.login !=
'dependabot[bot]'`), so new dependency versions' install and build scripts run
only on GitHub-hosted machines and never reach the toolchains and caches that
persist on the Studio.

**What a job can reach:**

- The runner is a standard macOS account (`arcci`) with no saved logins, keys
  or keychain items. Every routed job refuses to run if that account is an
  administrator.
- Routed jobs use no repository secrets and no deployment `environment:`.
  Their `GITHUB_TOKEN` is read-only, is never written to disk (checkout does
  not persist credentials), and expires when the job ends.
- Git's system configuration, which on macOS hands credentials to the
  keychain, is switched off for jobs (`GIT_CONFIG_NOSYSTEM=1`).
- Self-hosted jobs neither restore nor save the GitHub Actions cache, so
  nothing from the Studio can enter a cache that GitHub-hosted jobs restore.
- Jobs have the Studio's network access. Keep the Studio off networks that
  hold things CI should not reach.

**What persists between jobs:** the workspace is emptied before and after
every job, including `.git` and its hooks. These persist on purpose: the Rust
toolchains (`~/.rustup`), the cargo download cache (`~/.cargo`), the npm cache
(`~/.npm`) and the Node.js versions in the runner's tool cache. Any job from a
branch in this repository can change them, and later jobs, including those
for `main`, will use them. **A persistent runner is not a clean VM.**

**The release path never uses the Studio.** Release ARC, Release signing
preflight and Published artifact acceptance build, sign and check the
artifacts that users install. Some of those jobs hold signing keys (`environment: release`).
They run only on tags or by hand, so routing them would not shorten the pull
request queue. They stay on fresh GitHub-hosted machines. Never route a job
that uses secrets or `environment:`, or one that builds or publishes release
artifacts.

### Recommended runner-side guard (not installed by this repository)

A job-started hook runs on the runner before any workflow step. A pull request
cannot change it. This one refuses every job whose pull request comes from
another repository, whatever the workflow file says.

Save as `arc-runner-fork-guard.sh` on the Studio:

```bash
#!/bin/bash
# Refuses jobs for pull requests whose branch lives in another repository.
set -euo pipefail
/usr/bin/python3 - "${GITHUB_EVENT_PATH:?}" "${GITHUB_REPOSITORY:?}" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    event = json.load(handle)
pull_request = event.get("pull_request")
if pull_request is not None:
    head_repo = ((pull_request.get("head") or {}).get("repo") or {}).get("full_name")
    if head_repo != sys.argv[2]:
        sys.exit(f"ARC runner refuses a pull-request job from {head_repo!r}.")
PY
```

Install it root-owned, so jobs cannot edit it:

```sh
sudo install -d -o root -g wheel -m 0755 /usr/local/libexec
sudo install -o root -g wheel -m 0755 arc-runner-fork-guard.sh /usr/local/libexec/
```

Point the runner at it from the LaunchDaemon, which is also root-owned. In
`/Library/LaunchDaemons/com.arc.ci-runner.plist`, add:

```xml
<key>EnvironmentVariables</key>
<dict>
  <key>ACTIONS_RUNNER_HOOK_JOB_STARTED</key>
  <string>/usr/local/libexec/arc-runner-fork-guard.sh</string>
</dict>
```

Then restart the service, and run the Metal workflow once to confirm that jobs
still start:

```sh
sudo launchctl bootout system /Library/LaunchDaemons/com.arc.ci-runner.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.arc.ci-runner.plist
```

## What the self-hosted steps do

Every routed job has two extra steps. Both are skipped on GitHub-hosted
machines.

- **Prepare self-hosted runner** (first step):
  - refuses an administrator account;
  - empties the workspace;
  - sets `CARGO_HOME` and `RUSTUP_HOME` inside the runner account's home and
    puts `~/.cargo/bin` on `PATH`, so the toolchain installed by the first job
    is reused instead of re-downloaded;
  - sets `GIT_CONFIG_NOSYSTEM=1`.

  The first job installs rustup into the runner account's home through the
  pinned `dtolnay/rust-toolchain` action. An administrator can pre-install
  rustup for that account instead.
- **Clean up self-hosted runner** (last step, runs even after failure or
  cancellation): empties the workspace again. It never fails the job. If
  something cannot be removed, the next job's preparation step stops with an
  error instead of building on leftovers.

On self-hosted runs, `Swatinem/rust-cache` and `actions/setup-node`'s npm cache
are switched off.

## Metal GPU job

`.github/workflows/gpu-metal.yml` runs the `arc-gpu` tests serially, then the
4096×4096 benchmark. The benchmark fails unless a real GPU with a compiled
Metal pipeline is present. The job is never a required check.

- **Pull request:** add the `gpu-metal` label to a pull request whose branch is
  in this repository. It runs again on each push while the label is on. Removing
  the label does not cancel a run in progress; cancel it in the Actions tab.
- **By hand:** Actions → Metal GPU (self-hosted) → Run workflow. The branch you
  pick must contain this workflow file, so rebase older branches on `main`
  first.
- **Runner offline:** the job waits in the queue (GitHub drops it after
  24 hours) and blocks nothing.
- The Studio runs one job at a time, so a long Metal run delays routed CI jobs
  queued behind it.

## Keeping the runner healthy

- **Prerequisites.** The routed jobs are expected to need only the Xcode
  Command Line Tools (`clang`, `git`, `python3`). On Apple Silicon the native
  Rust dependencies build without cmake or Homebrew. The smoke test and the
  first routed run confirm this on the real machine. If the full Xcode app is
  installed, accept its license once: `sudo xcodebuild -license accept`.
- **Sleep.** Stop the Studio from sleeping (`sudo pmset -a sleep 0`). A
  sleeping runner looks offline.
- **Capacity.** One runner service runs one job at a time. Each pull request
  push sends three routed jobs (`test`, `desktop Tauri`, `golden vectors`).
  If the Studio becomes the bottleneck, add runner services, each under its own
  macOS account, because concurrent jobs must not share one `~/.rustup`.
- **Updates.** A macOS or Xcode update changes the build environment. Turn the
  switch off, update, run the Metal workflow, then turn it back on.

## Routing another workflow

Copy the pattern exactly:

- the `runs-on` expression, keeping matrix values and job names unchanged;
- the prepare and clean-up steps;
- the cache conditions (`if: runner.environment != 'self-hosted'` on
  `Swatinem/rust-cache`; the conditional `cache:` input on `actions/setup-node`).

Never route a job that uses secrets or `environment:`, or one that builds or
publishes release artifacts. Custom runner labels must be listed in
`.github/actionlint.yaml`, or the required `workflow syntax (actionlint)`
check fails.

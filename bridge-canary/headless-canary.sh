#!/usr/bin/env bash
# THROWAWAY CANARY (scratch branch bridge-canary-v0713, never merged): one real v0.7.11 headless install consumes two PUBLISHED bridge
# releases in turn, through scripts/legacy-bridge/canary-consume.sh (exactly the steps the v0.7 updater performs, plus a digest check).
#
#   1. The unmodified v0.7.11 community installer installs the pinned v0.7.7 binary under systemd (its curl calls are answered by the
#      repository's fake-github shim, which makes "Latest" v0.7.7; the shim is used for the installer ONLY). Real v0.7 state is written.
#   2. canary-consume.sh --tag <step 1 tag>: the launcher of the previous bridge release bridges the install (v0.7.12 -> arc-node v0.8.11).
#   3. canary-consume.sh --tag <step 2 tag>: the new launcher replaces it (v0.7.13 -> arc-node v0.8.12). This is the case that matters for
#      installs that already bridged: same identity, same fresh data directory, same archive; only the pinned node changes.
#   3b. Optional third step (the rollback rehearsal): the previous launcher is consumed again, as an updater would after Latest is moved back. It cannot fail the run
#      (the verdict for steps 1 and 2 stands) but its result is recorded as rollback_rehearsal PASS / FAIL.
#   4. After each step the same assertions as the repository's headless acceptance: pinned binary running at stake 0 in a fresh bridge
#      directory, no consensus participation, no model, privacy-safe community registration flags, the v0.7 data byte-identical to the
#      moment systemd stopped v0.7 (ExecStartPre hook), archive "unchanged", no seed in anything the bridge wrote.
#   5. With live_network "allowed" (needs a recorded authorization in the config) the node also has to appear on the public scoreboard of
#      all six coordinators under node-<8 hex>. With "blocked" every live-network address is rejected for the whole run.
#
# Usage: headless-canary.sh <config.json> <evidence-dir>
set -Eeuo pipefail

config="$(realpath "$1")"
evidence="$(realpath -m "$2")"
repo_root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
here="$repo_root/tests/legacy-bridge"

legacy_tag=v0.7.11
legacy_node_tag=v0.7.7
legacy_node_sha256=1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c
asset=arc-node-linux-x86_64
live_ips=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31)

export USER="${USER:-$(id -un)}"
arc_dir="$HOME/.arc"
state="$evidence/fake-github"
shim_bin="$here/fake-github"
snapshots="$evidence/start-snapshots"
mkdir -p "$evidence" "$state/api" "$state/raw/main" "$snapshots"
: > "$state/requests.log"

log() { printf '\n== %s\n' "$*"; }
fail() { printf 'CANARY FAILURE: %s\n' "$*" >&2; exit 1; }
sha() { sha256sum "$1" | awk '{print $1}'; }

# ---------------------------------------------------------------- the config: validated before anything is touched
jq -e '.schema == "arc.bridge-canary.config.v1"' "$config" >/dev/null || fail "unsupported canary config schema"
live_network="$(jq -er '.live_network' "$config")"
case "$live_network" in blocked | allowed) ;; *) fail "live_network must be blocked or allowed" ;; esac
authorization="$(jq -er '.authorization' "$config")"
registration_wait="$(jq -er '.registration_wait_s | if type == "number" and . >= 0 and . <= 900 then floor else error("registration_wait_s") end' "$config")"
if [ "$live_network" = allowed ]; then
    [ -n "$authorization" ] || fail "live_network allowed needs the recorded authorization text in the config"
    [ "$registration_wait" -gt 0 ] || fail "a live run must wait for the scoreboard (registration_wait_s > 0)"
fi
steps_count="$(jq -er '.steps | length' "$config")"
case "$steps_count" in 2 | 3) ;; *) fail "two steps are required, and an optional third one: the rollback rehearsal" ;; esac
for i in $(seq 0 $((steps_count - 1))); do
    jq -e --argjson i "$i" '.steps[$i] | (.tag | test("^v0\\.7\\.[0-9]+$")) and (.launcher_sha256 | test("^[0-9a-f]{64}$")) and (.node_tag | test("^v0\\.8\\.[0-9]+$"))
        and (.node_version == (.node_tag | ltrimstr("v"))) and (.node_sha256 | test("^[0-9a-f]{64}$"))' "$config" >/dev/null || fail "step $i of the config is malformed"
done
step() { jq -er --argjson i "$1" ".steps[\$i].$2" "$config"; }
cp "$config" "$evidence/config.json"

main_pid() { systemctl show -p MainPID --value arc-node; }
wait_health() {
    local url="$1" seconds="$2"
    for _ in $(seq 1 "$seconds"); do
        if /usr/bin/curl -sf -m 2 "$url/health" >/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

publish_latest() {
    # GitHub's /releases/latest JSON as the v0.7 installer parses it, retagged. Used for the installer only.
    local tag="$1"
    python3 - "$evidence/real-releases-latest.json" "$tag" "$state/api/releases-latest.json" <<'PY'
import json, sys
real, tag, out = sys.argv[1], sys.argv[2], sys.argv[3]
release = json.load(open(real, encoding="utf-8"))
release["tag_name"] = tag
release["name"] = tag
release["html_url"] = f"https://github.com/FerrumVir/arc-chain/releases/tag/{tag}"
release["assets"] = []
open(out, "w", encoding="utf-8").write(json.dumps(release, indent=2) + "\n")
PY
    printf '%s\n' "$tag" > "$state/latest-tag"
}

seed=""
on_exit() {
    local status=$?
    set +e
    mkdir -p "$evidence/final"
    systemctl status arc-node --no-pager > "$evidence/final/arc-node.status" 2>&1
    cp "$arc_dir/auto-update.log" "$arc_dir/version.txt" "$evidence/final/" 2>/dev/null
    tail -n 400 "$arc_dir/node.log" > "$evidence/final/node.log.tail" 2>/dev/null
    if [ -d "$arc_dir/legacy-bridge" ]; then
        cp "$arc_dir/legacy-bridge/bridge.log" "$evidence/final/" 2>/dev/null
        find "$arc_dir/legacy-bridge" -maxdepth 3 \( -name 'bridge-state.json' -o -name 'v0.7-data-archive-*.json' \) \
            -exec cp {} "$evidence/final/" \; 2>/dev/null
        find "$arc_dir/legacy-bridge" -maxdepth 4 -printf '%M %u %s %p\n' > "$evidence/final/legacy-bridge.tree" 2>/dev/null
    fi
    sudo systemctl stop arc-node 2>/dev/null   # the node leaves the network when the canary ends
    for ip in "${live_ips[@]}"; do
        sudo iptables -D OUTPUT -d "$ip" -j REJECT 2>/dev/null
    done
    if [ -n "$seed" ]; then
        # the seed belongs to a throwaway runner, but it is still redacted from everything that gets uploaded
        python3 - "$evidence" "$seed" <<'PY'
import os, sys
root, seed = sys.argv[1], sys.argv[2].encode()
for directory, _, files in os.walk(root):
    for name in files:
        path = os.path.join(directory, name)
        try:
            data = open(path, "rb").read()
        except OSError:
            continue
        if seed in data:
            open(path, "wb").write(data.replace(seed, b"<redacted-v07-seed>"))
            print("redacted the v0.7 seed in", os.path.relpath(path, root))
PY
    fi
    exit "$status"
}
trap on_exit EXIT

git -C "$repo_root" fetch --no-tags --depth=1 origin "refs/tags/$legacy_tag:refs/tags/$legacy_tag"

if [ "$live_network" = blocked ]; then
    log "Network: every live-network address is rejected for the whole run"
    for ip in "${live_ips[@]}"; do
        sudo iptables -I OUTPUT -d "$ip" -j REJECT
    done
    if /usr/bin/curl -s -m 5 -o /dev/null "https://${live_ips[0]}/health"; then
        fail "the live network is still reachable"
    fi
else
    log "Network: LIVE network allowed for this run (stake-0 observer, compute off, no model, stopped at the end)"
    printf 'authorization: %s\n' "$authorization" | tee "$evidence/live-network-authorization.txt"
fi

log "Collect the released v0.7 sources and the pinned v0.7.7 binary"
legacy="$evidence/legacy-source"
mkdir -p "$legacy"
git -C "$repo_root" show "$legacy_tag:scripts/install-community-node.sh" > "$legacy/install-community-node.sh"
git -C "$repo_root" show "$legacy_tag:testnet-seeds.txt" > "$state/raw/main/testnet-seeds.txt"
git -C "$repo_root" show "$legacy_tag:genesis.toml" > "$state/raw/main/genesis.toml"
mkdir -p "$state/releases/$legacy_node_tag"
/usr/bin/curl -fsSL --retry 3 -o "$state/releases/$legacy_node_tag/$asset" \
    "https://github.com/FerrumVir/arc-chain/releases/download/$legacy_node_tag/$asset"
[ "$(sha "$state/releases/$legacy_node_tag/$asset")" = "$legacy_node_sha256" ] || fail "the v0.7.7 binary does not match its pinned digest"
/usr/bin/curl -fsS --retry 3 -o "$evidence/real-releases-latest.json" "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest"
real_latest="$(jq -r .tag_name "$evidence/real-releases-latest.json")"
printf 'live GitHub Latest at the start of the canary: %s\n' "$real_latest"

log "Instrument (never modify) the v0.7 unit the installer will write"
sudo mkdir -p /etc/systemd/system/arc-node.service.d
sudo tee /etc/systemd/system/arc-node.service.d/50-canary-snapshot.conf >/dev/null <<EOF
[Service]
ExecStartPre=/usr/bin/python3 $here/snapshot_tree.py hook --root $arc_dir/data --binary $arc_dir/bin/arc-node --out-dir $snapshots
EOF

log "Install exactly as a v0.7 operator did: the unmodified v0.7.11 installer (answered by the shim as Latest v0.7.7)"
publish_latest "$legacy_node_tag"
export ARC_FAKE_GITHUB_STATE="$state"
PATH="$shim_bin:$PATH" bash "$legacy/install-community-node.sh" > "$evidence/v07-install.log" 2>&1 \
    || { cat "$evidence/v07-install.log"; fail "the v0.7.11 installer failed"; }
grep -q 'Latest version: v0.7.7' "$evidence/v07-install.log"
sudo systemctl stop arc-updater.timer
[ "$(cat "$arc_dir/version.txt")" = 0.7.7 ] || fail "v0.7 version.txt is not 0.7.7"
[ "$(sha "$arc_dir/bin/arc-node")" = "$legacy_node_sha256" ] || fail "installed binary is not v0.7.7"
wait_health http://127.0.0.1:9944 60 || fail "the v0.7.7 node never answered /health"
sleep 15
[ -s "$arc_dir/data/state.wal" ] || fail "the v0.7.7 node wrote no state WAL"
seed="$(cat "$arc_dir/identity.seed")"
[ -n "$seed" ] || fail "the v0.7 installer created no identity seed"

# --------------------------------------------------------------------------- the assertions that follow every consumption
pre_bridge=""
first_address=""
first_node_dir=""
first_generation=""

assert_bridged() {
    local index="$1" tag launcher node_tag node_version node_sha bridge_version pid exe node_dir state_json address
    tag="$(step "$index" tag)"
    launcher="$(step "$index" launcher_sha256)"
    node_tag="$(step "$index" node_tag)"
    node_version="$(step "$index" node_version)"
    node_sha="$(step "$index" node_sha256)"
    bridge_version="${tag#v}"
    local out="$evidence/after-step-$index"
    mkdir -p "$out"

    log "Step $((index + 1)) assertions: launcher $tag running arc-node $node_tag"
    [ "$(cat "$arc_dir/version.txt")" = "$bridge_version" ] || fail "version.txt is not $bridge_version"
    [ "$(sha "$arc_dir/bin/arc-node")" = "$launcher" ] || fail "bin/arc-node is not the $tag launcher"
    wait_health http://127.0.0.1:9944 90 || fail "the bridged node does not answer /health"
    pid="$(main_pid)"
    exe="$(readlink "/proc/$pid/exe")"
    printf 'pid %s exe %s\n' "$pid" "$exe" | tee "$out/process.txt"
    [ "$exe" = "$arc_dir/legacy-bridge/releases/$node_tag/$asset" ] || fail "unexpected node executable $exe"
    [ "$(sha "$exe")" = "$node_sha" ] || fail "the running node is not the pinned $node_tag binary"
    tr '\0' '\n' < "/proc/$pid/cmdline" > "$out/bridged-node.argv"
    argv_value() { grep -A1 -x -- "$1" "$out/bridged-node.argv" | tail -n 1; }
    if [ "$(argv_value --stake)" != 0 ] || [ "$(argv_value --min-stake)" != 0 ]; then
        fail "the node is not stake 0"
    fi
    node_data="$(argv_value --data-dir)"
    case "$node_data" in
        "$arc_dir"/legacy-bridge/nodes/headless-*/data) ;;
        *) fail "the node data directory $node_data is not a fresh bridge directory" ;;
    esac
    for forbidden in --validator-seed --insecure-dev-validator-seed --shard-range --model --full-integer-worker --no-community; do
        if grep -qx -- "$forbidden" "$out/bridged-node.argv"; then
            fail "the bridged node was started with $forbidden"
        fi
    done
    grep -qx -- --community-mode "$out/bridged-node.argv" || fail "a privacy-safe node build must register as a stake-0 community observer"
    [ "$(grep -cx -- --community-rpc-url "$out/bridged-node.argv")" = 6 ] || fail "the node was not given the six pinned community origins"
    [ "$(argv_value --rpc)" = 127.0.0.1:9944 ] || fail "the node RPC is not loopback-only"
    /usr/bin/curl -sf http://127.0.0.1:9944/node/info > "$out/node-info.json"
    /usr/bin/curl -sf http://127.0.0.1:9944/health > "$out/health.json"
    jq -e '.stake == 0' "$out/node-info.json" >/dev/null || fail "/node/info does not report stake 0"
    jq -e --arg v "$node_version" '.version == $v' "$out/node-info.json" >/dev/null || fail "/node/info does not report version $node_version"
    jq -e '.chain_participation_enabled == false' "$out/health.json" >/dev/null || fail "the bridged node participates in consensus"
    [ ! -e "$arc_dir/data/genesis.network-hash" ] || fail "v0.8 touched the v0.7 data directory"
    node_dir="$(dirname "$node_data")"
    state_json="$node_dir/bridge-state.json"
    cp "$state_json" "$out/bridge-state.json"
    jq -e --arg bv "$bridge_version" --arg nt "$node_tag" \
        '.stake == 0 and .legacy_kind == "headless" and (.compute | startswith("off:")) and .community_registration == true
         and .bridge_version == $bv and .node_release_tag == $nt' "$state_json" >/dev/null || fail "bridge-state.json does not describe $tag / $node_tag"
    address="$(jq -r .node_address "$state_json")"
    jq -e --slurpfile info "$out/node-info.json" '.node_address == ($info[0].validator | ltrimstr("0x"))' "$state_json" >/dev/null \
        || fail "bridge-state address differs from /node/info"
    grep -q 'Your ARC node is upgrading to the new network' "$arc_dir/node.log"

    # the v0.7 data: byte-identical to the moment systemd stopped v0.7 (the first start by the first launcher)
    if [ -z "$pre_bridge" ]; then
        local first_launcher
        first_launcher="$(step 0 launcher_sha256)"
        for snapshot in "$snapshots"/start-*.json; do
            if [ "$(jq -r .binary_sha256 "$snapshot")" = "$first_launcher" ]; then
                pre_bridge="$snapshot"
                break
            fi
        done
        [ -n "$pre_bridge" ] || fail "no ExecStartPre snapshot was taken before the first launcher ran"
        jq -e '.entries | has("state.wal")' "$pre_bridge" >/dev/null || fail "the pre-bridge snapshot holds no WAL"
        cp "$pre_bridge" "$evidence/v07-data.pre-bridge.json"
    fi
    python3 "$here/snapshot_tree.py" snapshot --root "$arc_dir/data" --out "$out/v07-data.after.json"
    python3 "$here/snapshot_tree.py" compare "$pre_bridge" "$out/v07-data.after.json" || fail "the v0.7 data directory changed after the bridge started"
    "$arc_dir/bin/arc-node" --legacy-bridge-verify-archive --hash > "$out/verify-archive.txt" 2>&1 || fail "--legacy-bridge-verify-archive failed"
    grep -qi 'unchanged' "$out/verify-archive.txt" || fail "the v0.7 archive is not reported unchanged"
    "$arc_dir/bin/arc-node" --legacy-bridge-status > "$out/status.txt" 2>&1 || fail "--legacy-bridge-status failed"
    grep -Eq '^  stake: +0$' "$out/status.txt" || fail "status does not report stake 0"
    if grep -rqF -- "$seed" "$arc_dir/legacy-bridge" "$out/bridged-node.argv"; then
        fail "the v0.7 seed appears in bridge output"
    fi

    if [ "$index" = 0 ]; then
        first_address="$address"
        first_node_dir="$node_dir"
        first_generation="$(jq -r .archive_generation "$state_json")"
    else
        [ "$address" = "$first_address" ] || fail "the new launcher changed the node identity ($address != $first_address)"
        [ "$node_dir" = "$first_node_dir" ] || fail "the new launcher moved the node directory"
        [ "$(jq -r .archive_generation "$state_json")" = "$first_generation" ] || fail "the archive generation changed although the v0.7 data did not"
        [ "$(sha "$arc_dir/bin/arc-node.prev")" = "$(step $((index - 1)) launcher_sha256)" ] || fail "arc-node.prev is not the previous launcher"
    fi
    printf 'step %s ok: %s -> %s, address %s\n' "$((index + 1))" "$tag" "$node_tag" "$address" | tee -a "$evidence/steps.txt"
}

consume() {
    local index="$1" tag launcher
    tag="$(step "$index" tag)"
    launcher="$(step "$index" launcher_sha256)"
    log "Step $((index + 1)): canary-consume.sh --tag $tag (the v0.7 updater's steps with a digest check)"
    started=$(date +%s)
    bash "$repo_root/scripts/legacy-bridge/canary-consume.sh" --tag "$tag" --expect-sha256 "$launcher" --apply \
        > "$evidence/canary-consume-step-$((index + 1)).txt" 2>&1 \
        || { cat "$evidence/canary-consume-step-$((index + 1)).txt"; fail "canary-consume.sh failed for $tag"; }
    cat "$evidence/canary-consume-step-$((index + 1)).txt"
    printf 'step %s consumed in %ss\n' "$((index + 1))" "$(( $(date +%s) - started ))" | tee -a "$evidence/steps.txt"
    if grep -q 'rolling back' "$evidence/canary-consume-step-$((index + 1)).txt"; then
        fail "the 30-second health check rolled $tag back"
    fi
}

consume 0
assert_bridged 0
consume 1
assert_bridged 1

# --------------------------------------------------------------------------- the public scoreboard (live runs only)
scoreboard_result=NOT_RUN
if [ "$live_network" = allowed ]; then
    log "Public scoreboard: the node has to appear on all six coordinators (read-only GET /workers/scoreboard?worker_id=; /community/list is never called)"
    if python3 - "$first_address" "$registration_wait" "$evidence" "$repo_root/crates/arc-legacy-bridge/pins/active.json" <<'PY'
import json, sys, time, urllib.request
address, wait, evidence, pins_path = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
origins = json.load(open(pins_path, encoding="utf-8"))["community_rpc_origins"]
want = "node-" + address[:8]
deadline = time.time() + wait
log = open(f"{evidence}/scoreboard.jsonl", "a", encoding="utf-8")
seen = {}
while time.time() < deadline and len(seen) < len(origins):
    for origin in origins:
        if origin in seen:
            continue
        url = origin.rstrip("/") + "/workers/scoreboard?limit=5&worker_id=0x" + address
        request = urllib.request.Request(url, headers={"User-Agent": "bridge-canary-scoreboard/1", "Accept": "application/json"})
        row = None
        error = None
        try:
            with urllib.request.urlopen(request, timeout=8) as response:
                parsed = json.loads(response.read(1 << 20).decode("utf-8", "replace"))
            rows = parsed.get("workers") if isinstance(parsed, dict) else []
            row = next((r for r in rows or [] if isinstance(r, dict) and str(r.get("worker_id", "")).lower() == "0x" + address), None)
        except Exception as exc:  # a failed lookup is a recorded miss
            error = type(exc).__name__
        log.write(json.dumps({"t": int(time.time()), "origin_index": origins.index(origin), "found": row is not None, "name": row and row.get("name"), "error": error}) + "\n")
        log.flush()
        if row is not None:
            if row.get("name") != want:
                print(f"FAIL: coordinator {origins.index(origin)} shows the name {row.get('name')!r}, expected {want!r}", file=sys.stderr)
                sys.exit(1)
            seen[origin] = row.get("name")
    time.sleep(10)
print(f"scoreboard: {len(seen)} of {len(origins)} coordinators list {want}")
sys.exit(0 if len(seen) == len(origins) else 1)
PY
    then
        scoreboard_result=PASS
    else
        scoreboard_result=FAIL
        printf 'SCOREBOARD CHECK FAILED: the node did not appear under its privacy-safe name on all six coordinators (see scoreboard.jsonl)\n' | tee -a "$evidence/steps.txt"
    fi
fi

rollback_rehearsal=NOT_RUN
if [ "$steps_count" = 3 ]; then
    log "Rollback rehearsal: Latest goes back to $(step 2 tag) ($(step 2 node_tag)); an install that already took $(step 1 tag) follows it. Same data, same identity."
    set +e
    ( set -e; consume 2; assert_bridged 2 )
    rehearsal_status=$?
    set -e
    if [ "$rehearsal_status" -eq 0 ]; then
        rollback_rehearsal=PASS
    else
        rollback_rehearsal=FAIL
        printf 'ROLLBACK REHEARSAL FAILED (exit %s): going back to %s on data written by %s is NOT safe\n' "$rehearsal_status" "$(step 2 tag)" "$(step 1 tag)" | tee -a "$evidence/steps.txt"
    fi
fi

verdict=CANARY_PASS
[ "$scoreboard_result" != FAIL ] || verdict=CANARY_FAIL
python3 - "$evidence" "$rollback_rehearsal" "$scoreboard_result" "$verdict" <<'PY'
import json, sys
evidence, rehearsal, scoreboard, verdict = sys.argv[1:5]
result = {"schema": "arc.bridge-canary.result.v1", "verdict": verdict, "rollback_rehearsal": rehearsal, "scoreboard": scoreboard, "steps": open(f"{evidence}/steps.txt", encoding="utf-8").read().splitlines()}
open(f"{evidence}/canary-result.json", "w", encoding="utf-8").write(json.dumps(result, indent=1) + "\n")
print(json.dumps(result, indent=1))
PY
[ "$verdict" = CANARY_PASS ] || fail "$verdict: scoreboard $scoreboard_result (steps 1 and 2 passed; the rollback rehearsal is $rollback_rehearsal)"
log "PASS: CANARY_PASS ($(step 0 tag) then $(step 1 tag)); scoreboard: $scoreboard_result; rollback rehearsal: $rollback_rehearsal"

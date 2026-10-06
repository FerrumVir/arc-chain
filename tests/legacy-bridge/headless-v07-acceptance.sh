#!/usr/bin/env bash
# Acceptance: a real v0.7.11 headless community install consumes the v0.7.12
# bridge through its own, unmodified updater and systemd units.
#
#   1. The unmodified v0.7.11 scripts/install-community-node.sh installs the
#      pinned v0.7.7 release binary (the last v0.7 release that shipped
#      headless binaries) as a systemd service, exactly as in the field.
#      The v0.7 node runs and writes genuine v0.7 state.
#   2. The fake-github curl shim makes "Latest" the v0.7.12 bridge release.
#      The installer-generated arc-auto-update.sh then runs as the real
#      arc-updater.service: it downloads the bridge, keeps arc-node.prev,
#      restarts arc-node.service, and applies its own 30-second health check.
#   3. Assertions: the v0.7 data directory is byte-identical to its state at
#      the moment systemd stopped v0.7 (captured by an ExecStartPre hook); the
#      running process is the pinned, signature-verified v0.8 binary at stake
#      0 in a fresh data directory, not participating in consensus; no seed
#      leaks; re-runs are idempotent; a corrupted cache self-heals; rollback
#      restores v0.7 on its untouched data; reinstall bridges again.
#
# Network: every live-network IP is rejected for the whole test, so neither
# v0.7 nor v0.8 can register with or mutate the public testnet. github.com
# stays reachable because the bridge downloads the real pinned v0.8 release.
#
# Usage: headless-v07-acceptance.sh <bridge-launcher> <evidence-dir>
set -Eeuo pipefail

bridge_binary="$(realpath "$1")"
evidence="$(realpath -m "$2")"
repo_root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
here="$repo_root/tests/legacy-bridge"

legacy_tag=v0.7.11
legacy_node_tag=v0.7.7
legacy_node_sha256=1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c
bridge_tag=v0.7.12
asset=arc-node-linux-x86_64
live_ips=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31)

pins="$repo_root/crates/arc-legacy-bridge/pins/active.json"
node_tag="$(jq -er '.node_release.tag' "$pins")"
node_version="$(jq -er '.node_release.version' "$pins")"
node_sha256="$(jq -er --arg a "$asset" '.node_release.assets[$a].sha256' "$pins")"

export USER="${USER:-$(id -un)}"
arc_dir="$HOME/.arc"
state="$evidence/fake-github"
shim_bin="$here/fake-github"
snapshots="$evidence/start-snapshots"
mkdir -p "$evidence" "$state/api" "$state/raw/main" "$snapshots"
: > "$state/requests.log"

log() { printf '\n== %s\n' "$*"; }
fail() { printf 'ACCEPTANCE FAILURE: %s\n' "$*" >&2; exit 1; }
sha() { sha256sum "$1" | awk '{print $1}'; }

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

main_pid() { systemctl show -p MainPID --value arc-node; }

publish_latest() {
    # GitHub's /releases/latest JSON as the v0.7 updater parses it: pretty
    # printed, one key per line. Captured live (read-only) and retagged.
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
    grep -q "\"tag_name\": \"$tag\"" "$state/api/releases-latest.json"
}

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
    for ip in "${live_ips[@]}"; do
        sudo iptables -D OUTPUT -d "$ip" -j REJECT 2>/dev/null
    done
    exit "$status"
}
trap on_exit EXIT

git -C "$repo_root" fetch --no-tags --depth=1 origin "refs/tags/$legacy_tag:refs/tags/$legacy_tag"

log "Isolate this runner from every live-network address"
seed_ips="$(
    {
        git -C "$repo_root" show "$legacy_tag:testnet-seeds.txt"
        git -C "$repo_root" show "$legacy_tag:genesis.toml"
        cat "$repo_root/testnet-seeds.txt" "$repo_root/genesis.toml"
        jq -r '.community_rpc_origins[]' "$pins"
    } | grep -oE '\b[0-9]{1,3}(\.[0-9]{1,3}){3}\b' | sort -u
)"
while read -r ip; do
    [ -n "$ip" ] || continue
    printf '%s\n' "${live_ips[@]}" | grep -qx "$ip" || fail "live address $ip is not in the block list"
done <<< "$seed_ips"
for ip in "${live_ips[@]}"; do
    sudo iptables -I OUTPUT -d "$ip" -j REJECT
done
if /usr/bin/curl -s -m 5 -o /dev/null "https://${live_ips[0]}/health"; then
    fail "the live network is still reachable"
fi

log "Collect the released v0.7 sources and the pinned v0.7.7 binary"
legacy="$evidence/legacy-source"
mkdir -p "$legacy"
git -C "$repo_root" show "$legacy_tag:scripts/install-community-node.sh" > "$legacy/install-community-node.sh"
git -C "$repo_root" show "$legacy_tag:testnet-seeds.txt" > "$state/raw/main/testnet-seeds.txt"
git -C "$repo_root" show "$legacy_tag:genesis.toml" > "$state/raw/main/genesis.toml"
git -C "$repo_root" rev-parse "$legacy_tag" > "$legacy/tag-commit.txt"
mkdir -p "$state/releases/$legacy_node_tag" "$state/releases/$bridge_tag"
/usr/bin/curl -fsSL --retry 3 -o "$state/releases/$legacy_node_tag/$asset" \
    "https://github.com/FerrumVir/arc-chain/releases/download/$legacy_node_tag/$asset"
[ "$(sha "$state/releases/$legacy_node_tag/$asset")" = "$legacy_node_sha256" ] \
    || fail "the v0.7.7 binary does not match its pinned digest"
/usr/bin/curl -fsS --retry 3 -o "$evidence/real-releases-latest.json" \
    "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest"
printf 'live GitHub Latest today: %s\n' "$(jq -r .tag_name "$evidence/real-releases-latest.json")"
cp "$bridge_binary" "$state/releases/$bridge_tag/$asset"
chmod 0755 "$state/releases/$bridge_tag/$asset"
bridge_sha="$(sha "$bridge_binary")"

log "Instrument (never modify) the v0.7 units the installer will write"
sudo mkdir -p /etc/systemd/system/arc-node.service.d /etc/systemd/system/arc-updater.service.d
sudo tee /etc/systemd/system/arc-node.service.d/50-acceptance-snapshot.conf >/dev/null <<EOF
[Service]
ExecStartPre=/usr/bin/python3 $here/snapshot_tree.py hook --root $arc_dir/data --binary $arc_dir/bin/arc-node --out-dir $snapshots
EOF
sudo tee /etc/systemd/system/arc-updater.service.d/50-acceptance-fake-github.conf >/dev/null <<EOF
[Service]
Environment=PATH=$shim_bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=ARC_FAKE_GITHUB_STATE=$state
EOF

log "Install exactly as a v0.7 operator did: the unmodified v0.7.11 installer"
publish_latest "$legacy_node_tag"
export ARC_FAKE_GITHUB_STATE="$state"
PATH="$shim_bin:$PATH" bash "$legacy/install-community-node.sh" > "$evidence/v07-install.log" 2>&1 \
    || { cat "$evidence/v07-install.log"; fail "the v0.7.11 installer failed"; }
grep -q 'Latest version: v0.7.7' "$evidence/v07-install.log"
sudo systemctl stop arc-updater.timer
[ "$(cat "$arc_dir/version.txt")" = 0.7.7 ] || fail "v0.7 version.txt is not 0.7.7"
[ "$(sha "$arc_dir/bin/arc-node")" = "$legacy_node_sha256" ] || fail "installed binary is not v0.7.7"
grep -q -- '--stake 0 --min-stake 0' /etc/systemd/system/arc-node.service
grep -q -- '--data-dir '"$arc_dir"'/data' /etc/systemd/system/arc-node.service
wait_health http://127.0.0.1:9944 60 || fail "the v0.7.7 node never answered /health"
sleep 15
[ -s "$arc_dir/data/state.wal" ] || fail "the v0.7.7 node wrote no state WAL"
v07_pid="$(main_pid)"
[ "$(readlink "/proc/$v07_pid/exe")" = "$arc_dir/bin/arc-node" ] || fail "v0.7 is not the running node"
cp /etc/systemd/system/arc-node.service "$evidence/arc-node.service"
seed="$(cat "$arc_dir/identity.seed")"
[ -n "$seed" ] || fail "the v0.7 installer created no identity seed"

log "Publish the bridge as Latest and run the real v0.7 updater"
publish_latest "$bridge_tag"
started=$(date +%s)
sudo systemctl start arc-updater.service
printf 'updater finished after %ss\n' "$(( $(date +%s) - started ))"
cp "$arc_dir/auto-update.log" "$evidence/auto-update.after-bridge.log"
grep -q 'new version available: 0.7.7 → 0.7.12' "$arc_dir/auto-update.log" \
    || fail "the v0.7 updater did not see the bridge release"
grep -q 'binary updated to v0.7.12' "$arc_dir/auto-update.log" || fail "the v0.7 updater did not install the bridge"
if grep -q 'ROLLED BACK' "$arc_dir/auto-update.log"; then
    fail "the v0.7 updater's 30-second health check rolled the bridge back"
fi
grep -q 'auto-update complete' "$arc_dir/auto-update.log"
[ "$(cat "$arc_dir/version.txt")" = 0.7.12 ]
[ "$(sha "$arc_dir/bin/arc-node")" = "$bridge_sha" ] || fail "bin/arc-node is not the bridge launcher"
[ "$(sha "$arc_dir/bin/arc-node.prev")" = "$legacy_node_sha256" ] || fail "arc-node.prev is not v0.7.7"

log "The running node is the pinned v0.8 binary, stake 0, fresh state"
wait_health http://127.0.0.1:9944 60 || fail "the bridged node does not answer /health"
pid="$(main_pid)"
exe="$(readlink "/proc/$pid/exe")"
printf 'pid %s exe %s\n' "$pid" "$exe"
[ "$exe" = "$arc_dir/legacy-bridge/releases/$node_tag/$asset" ] || fail "unexpected node executable $exe"
[ "$(sha "$exe")" = "$node_sha256" ] || fail "the running node is not the pinned $node_tag binary"
tr '\0' '\n' < "/proc/$pid/cmdline" > "$evidence/bridged-node.argv"
argv_value() { grep -A1 -x -- "$1" "$evidence/bridged-node.argv" | tail -n 1; }
if [ "$(argv_value --stake)" != 0 ] || [ "$(argv_value --min-stake)" != 0 ]; then
    fail "the node is not stake 0"
fi
node_data="$(argv_value --data-dir)"
case "$node_data" in
    "$arc_dir"/legacy-bridge/nodes/headless-*/data) ;;
    *) fail "the node data directory $node_data is not a fresh bridge directory" ;;
esac
[ "$node_data" != "$arc_dir/data" ]
for forbidden in --validator-seed --insecure-dev-validator-seed --shard-range --model; do
    if grep -qx -- "$forbidden" "$evidence/bridged-node.argv"; then
        fail "the bridged node was started with $forbidden"
    fi
done
grep -qx -- --no-community "$evidence/bridged-node.argv" \
    || fail "a node build that publishes hostnames must run without community registration"
[ "$(argv_value --rpc)" = 127.0.0.1:9944 ] || fail "the node RPC is not loopback-only"
/usr/bin/curl -sf http://127.0.0.1:9944/node/info > "$evidence/node-info.json"
/usr/bin/curl -sf http://127.0.0.1:9944/health > "$evidence/health.json"
jq -e '.stake == 0' "$evidence/node-info.json" >/dev/null || fail "/node/info does not report stake 0"
jq -e --arg v "$node_version" '.version == $v' "$evidence/node-info.json" >/dev/null || fail "/node/info version"
jq -e '.chain_participation_enabled == false' "$evidence/health.json" >/dev/null \
    || fail "the bridged node participates in consensus"
[ -s "$node_data/genesis.network-hash" ] || fail "v0.8 did not initialize its own fresh data directory"
[ ! -e "$arc_dir/data/genesis.network-hash" ] || fail "v0.8 touched the v0.7 data directory"
node_dir="$(dirname "$node_data")"
state_json="$node_dir/bridge-state.json"
jq -e '.stake == 0 and .legacy_kind == "headless" and (.compute | startswith("off:"))' "$state_json" >/dev/null
jq -e --slurpfile info "$evidence/node-info.json" '.node_address == ($info[0].validator | ltrimstr("0x"))' \
    "$state_json" >/dev/null || fail "bridge-state address differs from /node/info"
grep -q 'Your ARC node is upgrading to the new network' "$arc_dir/node.log"

log "The v0.7 data is byte-identical to the moment systemd stopped v0.7"
pre_bridge=""
for snapshot in "$snapshots"/start-*.json; do
    if [ "$(jq -r .binary_sha256 "$snapshot")" = "$bridge_sha" ] && [ -z "$pre_bridge" ]; then
        pre_bridge="$snapshot"
    fi
done
[ -n "$pre_bridge" ] || fail "no ExecStartPre snapshot was taken before the bridge first ran"
python3 "$here/snapshot_tree.py" snapshot --root "$arc_dir/data" --out "$evidence/v07-data.after-bridge.json"
python3 "$here/snapshot_tree.py" compare "$pre_bridge" "$evidence/v07-data.after-bridge.json" \
    || fail "the v0.7 data directory changed after the bridge started"
jq -e '.entries | has("state.wal")' "$pre_bridge" >/dev/null || fail "the pre-bridge snapshot holds no WAL"
cp "$pre_bridge" "$evidence/v07-data.pre-bridge.json"

log "No v0.7 seed leaks into anything the bridge wrote or started"
if grep -rqF -- "$seed" "$arc_dir/legacy-bridge" "$evidence/bridged-node.argv"; then
    fail "the v0.7 seed appears in bridge output"
fi
"$arc_dir/bin/arc-node" --legacy-bridge-verify-archive --hash > "$evidence/verify-archive.txt"
"$arc_dir/bin/arc-node" --legacy-bridge-status > "$evidence/status.txt"
grep -Eq '^  stake: +0$' "$evidence/status.txt" || fail "status does not report stake 0"

log "Idempotent: the updater sees nothing new; a restart reuses the verified cache"
address="$(jq -r .node_address "$state_json")"
sudo systemctl start arc-updater.service
tail -n 3 "$arc_dir/auto-update.log" | grep -q 'up to date (0.7.12)' || fail "the second updater run was not a no-op"
sudo systemctl restart arc-node
wait_health http://127.0.0.1:9944 60 || fail "the node did not come back after a restart"
grep -q "reusing the verified $node_tag release cache" "$arc_dir/legacy-bridge/bridge.log"
[ "$(jq -r .node_address "$state_json")" = "$address" ] || fail "a restart changed the node identity"
python3 "$here/snapshot_tree.py" snapshot --root "$arc_dir/data" --out "$evidence/v07-data.after-restart.json"
python3 "$here/snapshot_tree.py" compare "$pre_bridge" "$evidence/v07-data.after-restart.json"

log "A corrupted cached binary is detected and replaced, never executed"
sudo systemctl stop arc-node
printf 'x' >> "$arc_dir/legacy-bridge/releases/$node_tag/$asset"
sudo systemctl start arc-node
wait_health http://127.0.0.1:9944 90 || fail "the node did not recover from a corrupted cache"
[ "$(sha "$(readlink "/proc/$(main_pid)/exe")")" = "$node_sha256" ]
grep -q "cached $asset does not match its pinned digest" "$arc_dir/legacy-bridge/bridge.log"

log "Compute stays off without a verified model; validator command lines are refused"
printf 'not the model' > "$evidence/fake.gguf"
set +e
"$arc_dir/bin/arc-node" --legacy-bridge-compute on --model "$evidence/fake.gguf" > "$evidence/compute-on.txt" 2>&1
compute_status=$?
"$arc_dir/bin/arc-node" --rpc 0.0.0.0:9944 --p2p-port 9945 --data-dir "$arc_dir/data" \
    --validator-seed redacted --stake 5000000 --min-stake 500000 > "$evidence/validator.txt" 2>&1
validator_status=$?
set -e
[ "$compute_status" = 78 ] || fail "an unverified model was accepted (exit $compute_status)"
[ ! -e "$node_dir/compute-consent" ] || fail "consent was recorded for an unverified model"
[ "$validator_status" = 78 ] || fail "an explicit validator stake was not refused (exit $validator_status)"
"$arc_dir/bin/arc-node" --legacy-bridge-compute off > "$evidence/compute-off.txt"
[ "$(cat "$node_dir/compute-consent")" = no ]

log "Rollback restores v0.7 on its untouched data; reinstall bridges again"
"$arc_dir/bin/arc-node" --legacy-bridge-rollback > "$evidence/rollback.txt"
[ "$(sha "$arc_dir/bin/arc-node")" = "$legacy_node_sha256" ] || fail "rollback did not restore v0.7.7"
sudo systemctl restart arc-node
wait_health http://127.0.0.1:9944 60 || fail "v0.7.7 did not start after rollback"
[ "$(readlink "/proc/$(main_pid)/exe")" = "$arc_dir/bin/arc-node" ]
/usr/bin/curl -sf http://127.0.0.1:9944/health > "$evidence/health.after-rollback.json"
# The restored v0.7 node owns its data again and may write to it. Observe
# whether it does, so the archive expectation below matches what happened.
v07_wrote=false
for _ in $(seq 1 90); do
    python3 "$here/snapshot_tree.py" snapshot --root "$arc_dir/data" --out "$evidence/v07-data.after-rollback.json"
    if ! python3 "$here/snapshot_tree.py" compare "$pre_bridge" "$evidence/v07-data.after-rollback.json" >/dev/null; then
        v07_wrote=true
        break
    fi
    sleep 1
done
printf 'restored v0.7 node wrote to its data after rollback: %s\n' "$v07_wrote"
kept="$arc_dir/legacy-bridge/arc-node-bridge-0.7.12"
[ "$(sha "$kept")" = "$bridge_sha" ] || fail "rollback did not keep the bridge launcher"
# The running v0.7.7 binary is busy; stage and rename, as the updater does.
cp "$kept" "$arc_dir/bin/arc-node.new"
mv "$arc_dir/bin/arc-node.new" "$arc_dir/bin/arc-node"
sudo systemctl restart arc-node
wait_health http://127.0.0.1:9944 60 || fail "the node did not come back after reinstall"
[ "$(sha "$(readlink "/proc/$(main_pid)/exe")")" = "$node_sha256" ]
[ "$(jq -r .node_address "$state_json")" = "$address" ] || fail "reinstall changed the node identity"
expected_generation=1
if [ "$v07_wrote" = true ]; then
    expected_generation=2
fi
jq -e --argjson g "$expected_generation" '.archive_generation == $g' "$state_json" >/dev/null \
    || fail "archive generation $(jq -r .archive_generation "$state_json") does not reflect the v0.7 run after rollback (expected $expected_generation)"
if [ "$expected_generation" = 2 ]; then
    grep -q 'the v0.7 data changed since archive record 1' "$arc_dir/legacy-bridge/bridge.log" \
        || fail "the bridge did not log that v0.7 changed its data after the rollback"
fi

log "PASS: v0.7.11 headless install bridged to $node_tag at stake 0; v0.7 data untouched"

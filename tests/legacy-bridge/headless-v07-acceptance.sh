#!/usr/bin/env bash
set -Eeuo pipefail

bridge_binary="$(realpath "$1")"
evidence="$(realpath -m "$2")"
repo_root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
here="$repo_root/tests/legacy-bridge"

legacy_tag=v0.7.11
legacy_node_tag=v0.7.7
legacy_node_sha256=1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c
asset=arc-node-linux-x86_64
live_ips=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31)

pins="$repo_root/crates/arc-legacy-bridge/pins/active.json"
node_tag="$(jq -er '.node_release.tag' "$pins")"
bridge_version="$(jq -er '.bridge_version' "$pins")"
bridge_tag="v$bridge_version"
node_version="$(jq -er '.node_release.version' "$pins")"
node_sha256="$(jq -er --arg a "$asset" '.node_release.assets[$a].sha256' "$pins")"
privacy_safe="$(jq -er '.node_release.worker_names_privacy_safe | if type == "boolean" then tostring else error("privacy flag must be boolean") end' "$pins")"
scenario="${ARC_BRIDGE_ACCEPTANCE_SCENARIO:-fresh}"
case "$scenario" in
    fresh|previous-bridge) ;;
    *) printf 'Unknown bridge acceptance scenario: %s\n' "$scenario" >&2; exit 1 ;;
esac
updater_from_version=0.7.7
previous_executable_sha256="$legacy_node_sha256"

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

local_curl() {
    local timeout="${1:-5}"
    shift
    /usr/bin/curl -sf --noproxy '*' --max-redirs 0 --connect-timeout 2 -m "$timeout" "$@"
}

wait_health() {
    local url="$1" seconds="$2" deadline
    deadline=$(( $(date +%s) + seconds ))
    while true; do
        if local_curl 2 "$url/health" >/dev/null; then
            return 0
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 1
    done
}

wait_node_info() {
    local output="$1" expected_version="$2" expected_address="$3" seconds="${4:-600}" deadline
    deadline=$(( $(date +%s) + seconds ))
    while true; do
        if local_curl 2 http://127.0.0.1:9944/node/info > "$output"; then
            if jq -e --arg version "$expected_version" --arg address "$expected_address" \
                '.stake == 0 and .version == $version and (.validator | ltrimstr("0x")) == $address' \
                "$output" >/dev/null; then
                return 0
            fi
            fail "worker completed startup with the wrong version, stake or identity"
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 1
    done
}

main_pid() { systemctl show -p MainPID --value arc-node; }

argv_value_in() {
    local file="$1" flag="$2"
    awk -v flag="$flag" 'found { print; exit } $0 == flag { found = 1 }' "$file"
}

assert_regular_file() {
    local path="$1" context="$2"
    if [ -L "$path" ] || [ ! -f "$path" ]; then
        fail "$context: expected a regular non-symlink file at $path"
    fi
}

assert_absent_path() {
    local path="$1" context="$2"
    if [ -e "$path" ] || [ -L "$path" ]; then
        fail "$context: unexpected file or symlink at $path"
    fi
}

assert_wal_prefix_preserved() {
    local before_wal="$1" current_wal="$2" context="$3"
    [ -s "$current_wal" ] || fail "$context: missing or empty state WAL at $current_wal"
    [ "$(stat -c %s "$current_wal")" -le 8388608 ] || fail "$context: isolated WAL exceeds 8 MiB"
    python3 - "$before_wal" "$current_wal" "$context" <<'PY'
import sys
from pathlib import Path
before = Path(sys.argv[1]).read_bytes()
with Path(sys.argv[2]).open("rb") as stream:
    after = stream.read(len(before))
if not before or before != after:
    raise SystemExit(f"{sys.argv[3]}: discarded or rewrote the persisted WAL prefix")
PY
}

assert_running_node() {
    local expected_tag="$1" expected_version="$2" expected_sha="$3" expected_address="$4" info_out="$5" argv_out="$6"
    local pid exe
    pid="$(main_pid)"
    if [ -z "$pid" ] || [ "$pid" = 0 ]; then
        fail "arc-node has no active systemd MainPID"
    fi
    exe="$(readlink "/proc/$pid/exe")"
    [ "$exe" = "$arc_dir/legacy-bridge/releases/$expected_tag/$asset" ] \
        || fail "unexpected running node executable $exe (expected $expected_tag)"
    [ "$(sha "$exe")" = "$expected_sha" ] \
        || fail "running node binary digest does not match $expected_tag"
    tr '\0' '\n' < "/proc/$pid/cmdline" > "$argv_out"
    [ "$(argv_value_in "$argv_out" --rpc)" = "127.0.0.1:9944" ] \
        || fail "running node RPC is not bound to 127.0.0.1:9944"
    local_curl 5 http://127.0.0.1:9944/node/info > "$info_out"
    jq -e --arg address "$expected_address" --arg version "$expected_version" \
        '.stake == 0 and .version == $version and (.validator | ltrimstr("0x")) == $address' \
        "$info_out" >/dev/null \
        || fail "running node /node/info did not match version $expected_version, stake 0, and identity $expected_address"
}

publish_latest() {
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
    local stop_ok=true
    local unit_state=""
    local pid=""
    local updater_pid=""
    local ip=""
    set +e
    mkdir -p "$evidence/final"
    systemctl status arc-node --no-pager > "$evidence/final/arc-node.status.before-stop" 2>&1
    if [ -f /etc/systemd/system/arc-updater.timer ]; then
        sudo systemctl stop arc-updater.timer >/dev/null 2>&1 || stop_ok=false
        unit_state="$(systemctl is-active arc-updater.timer 2>/dev/null)"
        if [ "$unit_state" != inactive ] && [ "$unit_state" != failed ]; then
            stop_ok=false
        fi
    fi
    if [ -f /etc/systemd/system/arc-updater.service ]; then
        sudo systemctl stop arc-updater.service >/dev/null 2>&1 || stop_ok=false
        unit_state="$(systemctl is-active arc-updater.service 2>/dev/null)"
        if [ "$unit_state" != inactive ] && [ "$unit_state" != failed ]; then
            stop_ok=false
        fi
        updater_pid="$(systemctl show -p MainPID --value arc-updater.service 2>/dev/null)"
        if [ "$updater_pid" != 0 ]; then
            stop_ok=false
        fi
    fi
    if [ -f /etc/systemd/system/arc-node.service ]; then
        sudo systemctl stop arc-node.service >/dev/null 2>&1 || stop_ok=false
        unit_state="$(systemctl is-active arc-node.service 2>/dev/null)"
        if [ "$unit_state" != inactive ] && [ "$unit_state" != failed ]; then
            stop_ok=false
        fi
        pid="$(systemctl show -p MainPID --value arc-node.service 2>/dev/null)"
        if [ "$pid" != 0 ]; then
            stop_ok=false
        fi
    fi
    systemctl status arc-node --no-pager > "$evidence/final/arc-node.status" 2>&1
    cp "$arc_dir/auto-update.log" "$arc_dir/version.txt" "$evidence/final/" 2>/dev/null
    tail -n 400 "$arc_dir/node.log" > "$evidence/final/node.log.tail" 2>/dev/null
    if [ -d "$arc_dir/legacy-bridge" ]; then
        cp "$arc_dir/legacy-bridge/bridge.log" "$evidence/final/" 2>/dev/null
        find "$arc_dir/legacy-bridge" -maxdepth 3 \( -name 'bridge-state.json' -o -name 'v0.7-data-archive-*.json' \) \
            -exec cp {} "$evidence/final/" \; 2>/dev/null
        find "$arc_dir/legacy-bridge" -maxdepth 4 -printf '%M %u %s %p\n' > "$evidence/final/legacy-bridge.tree" 2>/dev/null
    fi
    if [ "$stop_ok" = true ]; then
        for ip in "${live_ips[@]}"; do
            sudo iptables -D OUTPUT -d "$ip" -j REJECT 2>/dev/null
        done
    else
        printf 'ACCEPTANCE FAILURE: owned systemd services did not reach inactive MainPID=0; retaining live-network iptables block\n' >&2
        status=1
    fi
    exit "$status"
}
trap on_exit EXIT

git -C "$repo_root" fetch --no-tags --depth=1 origin "refs/tags/$legacy_tag:refs/tags/$legacy_tag"
git -C "$repo_root" show "$legacy_tag:testnet-seeds.txt" > "$state/raw/main/testnet-seeds.txt"
git -C "$repo_root" show "$legacy_tag:genesis.toml" > "$state/raw/main/genesis.toml"

log "Isolate this runner from every live-network address"
seed_ips="$(
    python3 - "$pins" \
        "$state/raw/main/testnet-seeds.txt" \
        "$state/raw/main/genesis.toml" \
        "$repo_root/testnet-seeds.txt" \
        "$repo_root/genesis.toml" <<'PY'
import ipaddress
import json
from pathlib import Path
import re
import sys
from urllib.parse import urlparse

pins = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
found = set()

def extract_strict_ipv4_host(origin, label):
    parsed = urlparse(origin)
    if parsed.scheme != "https" or not parsed.hostname:
        raise SystemExit(f"{label} must be an https IPv4 origin, got {origin!r}")
    try:
        addr = ipaddress.IPv4Address(parsed.hostname)
    except ipaddress.AddressValueError as error:
        raise SystemExit(f"{label} must use an explicit IPv4 address for iptables isolation, got {origin!r}") from error
    found.add(str(addr))

for origin in pins["community_rpc_origins"]:
    extract_strict_ipv4_host(origin, "community_rpc_origins entry")

shard_worker = pins.get("shard_worker")
if shard_worker is not None:
    if not isinstance(shard_worker, dict) or not isinstance(shard_worker.get("coordinator_url"), str):
        raise SystemExit("shard_worker.coordinator_url must be a string when shard_worker is present")
    extract_strict_ipv4_host(shard_worker["coordinator_url"], "shard_worker.coordinator_url")

for path in sys.argv[2:]:
    text = Path(path).read_text(encoding="utf-8")
    for match in re.findall(r"\b[0-9]{1,3}(?:\.[0-9]{1,3}){3}\b", text):
        try:
            addr = ipaddress.IPv4Address(match)
        except ipaddress.AddressValueError:
            continue
        found.add(str(addr))

for ip in sorted(found):
    print(ip)
PY
)"
[ -n "$seed_ips" ] || fail "failed to discover any live-network IPv4 addresses"
while read -r ip; do
    [ -n "$ip" ] || continue
    printf '%s\n' "${live_ips[@]}" | grep -qx "$ip" || fail "live address $ip is not in the block list"
done <<< "$seed_ips"
for ip in "${live_ips[@]}"; do
    sudo iptables -I OUTPUT -d "$ip" -j REJECT
done
if /usr/bin/curl -s --noproxy '*' --max-redirs 0 -m 5 -o /dev/null "https://${live_ips[0]}/health"; then
    fail "the live network is still reachable"
fi

log "Collect the released v0.7 sources and the pinned v0.7.7 binary"
legacy="$evidence/legacy-source"
mkdir -p "$legacy"
git -C "$repo_root" show "$legacy_tag:scripts/install-community-node.sh" > "$legacy/install-community-node.sh"
git -C "$repo_root" rev-parse "$legacy_tag" > "$legacy/tag-commit.txt"
mkdir -p "$state/releases/$legacy_node_tag" "$state/releases/$bridge_tag"
python3 "$here/fetch_release_metadata.py" --out "$evidence/real-releases-latest.json"
unset GH_TOKEN GITHUB_TOKEN
printf 'live GitHub Latest today: %s\n' "$(jq -r .tag_name "$evidence/real-releases-latest.json")"
/usr/bin/curl -fsSL --retry 3 -o "$state/releases/$legacy_node_tag/$asset" \
    "https://github.com/FerrumVir/arc-chain/releases/download/$legacy_node_tag/$asset"
[ "$(sha "$state/releases/$legacy_node_tag/$asset")" = "$legacy_node_sha256" ] \
    || fail "the v0.7.7 binary does not match its pinned digest"
cp "$bridge_binary" "$state/releases/$bridge_tag/$asset"
chmod 0755 "$state/releases/$bridge_tag/$asset"
bridge_sha="$(sha "$bridge_binary")"
first_bridge_sha="$bridge_sha"

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

if [ "$scenario" = previous-bridge ]; then
    log "Run the immutable previous bridge before qualifying an existing-install update"
    previous_release="$here/previous-release.json"
    [ "$(jq -er '.schema' "$previous_release")" = "arc.legacy-bridge.previous-release.v1" ] \
        || fail "unexpected previous-release fixture schema"
    prior_tag="$(jq -er '.bridge_tag' "$previous_release")"
    prior_version="${prior_tag#v}"
    prior_node_tag="$(jq -er '.node_tag' "$previous_release")"
    prior_node_version="${prior_node_tag#v}"
    prior_bridge_sha256="$(jq -er --arg a "$asset" '.assets[$a].sha256' "$previous_release")"
    prior_bridge_size="$(jq -er --arg a "$asset" '.assets[$a].size' "$previous_release")"
    prior_node_sha256=d4b19c031cf5423a8ca5ce6ddf2f3f3819b08b00032b33d00e94bf7937523f99
    mkdir -p "$state/releases/$prior_tag"
    /usr/bin/curl -fsSL --retry 3 -o "$state/releases/$prior_tag/$asset" \
        "https://github.com/FerrumVir/arc-chain/releases/download/$prior_tag/$asset"
    [ "$(stat -c %s "$state/releases/$prior_tag/$asset")" = "$prior_bridge_size" ] \
        || fail "the previous bridge size differs from $previous_release"
    [ "$(sha "$state/releases/$prior_tag/$asset")" = "$prior_bridge_sha256" ] \
        || fail "the previous bridge differs from $previous_release"
    chmod 0755 "$state/releases/$prior_tag/$asset"
    publish_latest "$prior_tag"
    sudo systemctl start arc-updater.service
    wait_health http://127.0.0.1:9944 60 || fail "the previous bridge did not start"
    [ "$(cat "$arc_dir/version.txt")" = "$prior_version" ] || fail "the previous bridge update rolled back"
    mapfile -t prior_states < <(find "$arc_dir/legacy-bridge/nodes" -maxdepth 2 -name bridge-state.json -type f)
    [ "${#prior_states[@]}" = 1 ] || fail "the previous bridge must create exactly one context"
    prior_state="${prior_states[0]}"
    prior_data="$(jq -er .node_data_dir "$prior_state")"
    prior_keyfile="$(jq -er .node_keyfile "$prior_state")"
    prior_address="$(jq -er .node_address "$prior_state")"
    assert_running_node "$prior_node_tag" "$prior_node_version" "$prior_node_sha256" "$prior_address" \
        "$evidence/node-info.prior-bridge.json" "$evidence/prior-bridge.argv"
    case "$prior_data" in
        "$arc_dir"/legacy-bridge/nodes/headless-*/data) ;;
        *) fail "the previous bridge used an unexpected data directory" ;;
    esac
    sudo systemctl stop arc-node
    "$arc_dir/bin/arc-node" --legacy-bridge-compute off > "$evidence/prior-compute-off.txt"
    prior_key_sha256="$(sha "$prior_keyfile")"
    prior_genesis_sha256="$(sha "$prior_data/genesis.network-hash")"
    [ -s "$prior_data/state.wal" ] || fail "the previous node has no persistent WAL to preserve"
    [ "$(stat -c %s "$prior_data/state.wal")" -le 8388608 ] || fail "the isolated fixture WAL exceeds 8 MiB"
    cp "$prior_data/state.wal" "$evidence/prior-bridge-state.wal"
    printf 'preserve-existing-bridge-data\n' > "$prior_data/bridge-upgrade-preservation.txt"
    cp "$prior_state" "$evidence/prior-bridge-state.json"
    updater_from_version="$prior_version"
    previous_executable_sha256="$prior_bridge_sha256"
    first_bridge_sha="$prior_bridge_sha256"
fi

log "Publish the bridge as Latest and run the real v0.7 updater"
publish_latest "$bridge_tag"
started=$(date +%s)
sudo systemctl start arc-updater.service
printf 'updater finished after %ss\n' "$(( $(date +%s) - started ))"
cp "$arc_dir/auto-update.log" "$evidence/auto-update.after-bridge.log"
grep -qF "new version available: $updater_from_version → $bridge_version" "$arc_dir/auto-update.log" \
    || fail "the v0.7 updater did not see the bridge release"
grep -qF "binary updated to v$bridge_version" "$arc_dir/auto-update.log" || fail "the v0.7 updater did not install the bridge"
if grep -q 'ROLLED BACK' "$arc_dir/auto-update.log"; then
    fail "the v0.7 updater's 30-second health check rolled the bridge back"
fi
grep -q 'auto-update complete' "$arc_dir/auto-update.log"
[ "$(cat "$arc_dir/version.txt")" = "$bridge_version" ]
[ "$(sha "$arc_dir/bin/arc-node")" = "$bridge_sha" ] || fail "bin/arc-node is not the bridge launcher"
[ "$(sha "$arc_dir/bin/arc-node.prev")" = "$previous_executable_sha256" ] || fail "arc-node.prev does not preserve the previous executable"

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
if [ "$privacy_safe" = true ]; then
    grep -qx -- --community-mode "$evidence/bridged-node.argv" \
        || fail "a privacy-safe node build must register as a stake-0 community observer"
    if grep -qx -- --no-community "$evidence/bridged-node.argv"; then
        fail "a privacy-safe node build was started without community registration"
    fi
    [ "$(grep -cx -- --community-rpc-url "$evidence/bridged-node.argv")" = "$(jq -er '.community_rpc_origins | length' "$pins")" ] \
        || fail "the node was not given every pinned community origin"
else
    grep -qx -- --no-community "$evidence/bridged-node.argv" \
        || fail "a node build that publishes hostnames must run without community registration"
fi
[ "$(argv_value --rpc)" = 127.0.0.1:9944 ] || fail "the node RPC is not loopback-only"
local_curl 5 http://127.0.0.1:9944/node/info > "$evidence/node-info.json"
local_curl 5 http://127.0.0.1:9944/health > "$evidence/health.json"
jq -e '.stake == 0' "$evidence/node-info.json" >/dev/null || fail "/node/info does not report stake 0"
jq -e --arg v "$node_version" '.version == $v' "$evidence/node-info.json" >/dev/null || fail "/node/info version"
jq -e '.chain_participation_enabled == false' "$evidence/health.json" >/dev/null \
    || fail "the bridged node participates in consensus"
[ -s "$node_data/genesis.network-hash" ] || fail "v0.8 did not initialize its own fresh data directory"
[ ! -e "$arc_dir/data/genesis.network-hash" ] || fail "v0.8 touched the v0.7 data directory"
node_dir="$(dirname "$node_data")"
state_json="$node_dir/bridge-state.json"
jq -e '.stake == 0 and .legacy_kind == "headless" and (.compute | startswith("off:"))' "$state_json" >/dev/null
jq -e --argjson safe "$privacy_safe" '.community_registration == $safe' "$state_json" >/dev/null \
    || fail "bridge-state community_registration does not follow the pin"
jq -e --slurpfile info "$evidence/node-info.json" '.node_address == ($info[0].validator | ltrimstr("0x"))' \
    "$state_json" >/dev/null || fail "bridge-state address differs from /node/info"
grep -q 'Your ARC node is upgrading to the new network' "$arc_dir/node.log"

if [ "$scenario" = previous-bridge ]; then
    log "The update preserves the existing bridge identity, state and compute choice"
    [ "$node_data" = "$prior_data" ] || fail "the update replaced the existing bridge data directory"
    [ "$state_json" = "$prior_state" ] || fail "the update replaced the existing bridge context"
    [ "$(jq -er .node_address "$state_json")" = "$prior_address" ] || fail "the update changed the bridge identity"
    [ "$(sha "$prior_keyfile")" = "$prior_key_sha256" ] || fail "the update rewrote the existing private key"
    [ "$(sha "$node_data/genesis.network-hash")" = "$prior_genesis_sha256" ] || fail "the update changed the persisted genesis binding"
    [ "$(cat "$node_dir/compute-consent")" = no ] || fail "the update discarded the saved compute opt-out"
    [ "$(cat "$node_data/bridge-upgrade-preservation.txt")" = preserve-existing-bridge-data ] || fail "the update discarded existing data"
    assert_wal_prefix_preserved "$evidence/prior-bridge-state.wal" "$node_data/state.wal" "existing bridge update"
    cp "$state_json" "$evidence/updated-bridge-state.json"
fi

log "The v0.7 data is byte-identical to the moment systemd stopped v0.7"
pre_bridge=""
for snapshot in "$snapshots"/start-*.json; do
    if [ "$(jq -r .binary_sha256 "$snapshot")" = "$first_bridge_sha" ] && [ -z "$pre_bridge" ]; then
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
tail -n 3 "$arc_dir/auto-update.log" | grep -qF "up to date ($bridge_version)" || fail "the second updater run was not a no-op"
sudo systemctl restart arc-node
wait_health http://127.0.0.1:9944 60 || fail "the node did not come back after a restart"
grep -q "reusing the verified $node_tag release cache" "$arc_dir/legacy-bridge/bridge.log"
[ "$(jq -r .node_address "$state_json")" = "$address" ] || fail "a restart changed the node identity"
python3 "$here/snapshot_tree.py" snapshot --root "$arc_dir/data" --out "$evidence/v07-data.after-restart.json"
python3 "$here/snapshot_tree.py" compare "$pre_bridge" "$evidence/v07-data.after-restart.json"

if [ "$scenario" = previous-bridge ]; then
    log "The previous bridge reopens the upgraded data and the real updater recovers again"
    sudo systemctl stop arc-node
    install -m 0755 "$state/releases/$prior_tag/$asset" "$arc_dir/bin/arc-node"
    printf '%s\n' "$prior_version" > "$arc_dir/version.txt"
    sudo systemctl start arc-node
    wait_health http://127.0.0.1:9944 30 || fail "the previous bridge cannot reopen upgraded data"
    assert_running_node "$prior_node_tag" "$prior_node_version" "$prior_node_sha256" "$prior_address" \
        "$evidence/node-info.previous-bridge-rollback.json" "$evidence/previous-bridge-rollback.argv"
    [ "$(sha "$prior_keyfile")" = "$prior_key_sha256" ] || fail "rollback rewrote the private key"
    [ "$(sha "$node_data/genesis.network-hash")" = "$prior_genesis_sha256" ] \
        || fail "rollback changed the persisted genesis binding"
    assert_wal_prefix_preserved "$evidence/prior-bridge-state.wal" "$node_data/state.wal" "previous-bridge downgrade"
    [ "$(cat "$node_dir/compute-consent")" = no ] || fail "rollback discarded the saved compute opt-out"
    [ "$(sha "$arc_dir/legacy-bridge/preserved/arc-node.before-bridge")" = "$legacy_node_sha256" ] \
        || fail "the second-generation bridge replaced the original rollback binary"
    sudo systemctl start arc-updater.service
    wait_health http://127.0.0.1:9944 30 || fail "the updater did not recover after rollback"
    [ "$(cat "$arc_dir/version.txt")" = "$bridge_version" ] || fail "recovery rolled back again"
    [ "$(sha "$arc_dir/bin/arc-node")" = "$bridge_sha" ] || fail "recovery did not install the candidate bridge"
    assert_running_node "$node_tag" "$node_version" "$node_sha256" "$prior_address" \
        "$evidence/node-info.after-rollback-recovery.json" "$evidence/after-rollback-recovery.argv"
    [ "$(sha "$prior_keyfile")" = "$prior_key_sha256" ] || fail "recovery rewrote the private key"
    [ "$(sha "$node_data/genesis.network-hash")" = "$prior_genesis_sha256" ] \
        || fail "recovery changed the persisted genesis binding"
    assert_wal_prefix_preserved "$evidence/prior-bridge-state.wal" "$node_data/state.wal" "rollback recovery"
    [ "$(cat "$node_data/bridge-upgrade-preservation.txt")" = preserve-existing-bridge-data ] \
        || fail "rollback and recovery discarded existing data"
    cp "$arc_dir/auto-update.log" "$evidence/auto-update.after-rollback-recovery.log"
fi

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
if [ "$scenario" = previous-bridge ]; then
    [ "$(cat "$node_dir/compute-consent")" = no ] || fail "an unverified model replaced the saved compute opt-out"
else
    [ ! -e "$node_dir/compute-consent" ] || fail "consent was recorded for an unverified model"
fi
[ "$validator_status" = 78 ] || fail "an explicit validator stake was not refused (exit $validator_status)"
"$arc_dir/bin/arc-node" --legacy-bridge-compute off > "$evidence/compute-off.txt"
[ "$(cat "$node_dir/compute-consent")" = no ]

log "Rollback restores v0.7 on its untouched data; reinstall bridges again"
"$arc_dir/bin/arc-node" --legacy-bridge-rollback > "$evidence/rollback.txt"
[ "$(sha "$arc_dir/bin/arc-node")" = "$legacy_node_sha256" ] || fail "rollback did not restore v0.7.7"
sudo systemctl restart arc-node
wait_health http://127.0.0.1:9944 60 || fail "v0.7.7 did not start after rollback"
[ "$(readlink "/proc/$(main_pid)/exe")" = "$arc_dir/bin/arc-node" ]
local_curl 5 http://127.0.0.1:9944/health > "$evidence/health.after-rollback.json"
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
kept="$arc_dir/legacy-bridge/arc-node-bridge-$bridge_version"
[ "$(sha "$kept")" = "$bridge_sha" ] || fail "rollback did not keep the bridge launcher"
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

if [ "$scenario" = previous-bridge ]; then
    log "An opted-in existing worker must pass the unmodified updater's real health deadline"
    sudo systemctl stop arc-node
    available_kib="$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)"
    if ! [[ "$available_kib" =~ ^[0-9]+$ ]] || [ "$available_kib" -lt 12582912 ]; then
        fail "compute upgrade qualification needs a remote runner with 12 GiB available memory"
    fi
    disk_kib="$(df -Pk "$arc_dir" | awk 'END {print $4}')"
    if ! [[ "$disk_kib" =~ ^[0-9]+$ ]] || [ "$disk_kib" -lt 10485760 ]; then
        fail "compute upgrade qualification needs 10 GiB free disk"
    fi
    "$arc_dir/bin/arc-node" --legacy-bridge-worker-mode full > "$evidence/compute-worker-mode.full.txt"
    assert_regular_file "$node_dir/compute-worker-mode" "full-worker override setup"
    [ "$(cat "$node_dir/compute-worker-mode")" = full ] \
        || fail "full-worker override was not recorded as full"
    install -m 0755 "$state/releases/$prior_tag/$asset" "$arc_dir/bin/arc-node"
    printf '%s\n' "$prior_version" > "$arc_dir/version.txt"
    "$arc_dir/bin/arc-node" --legacy-bridge-compute on --download-model > "$evidence/prior-compute-on.txt" 2>&1
    assert_regular_file "$node_dir/compute-consent" "compute opt-in fixture"
    [ "$(cat "$node_dir/compute-consent")" = yes ] \
        || fail "the fixture did not retain explicit compute opt-in"
    assert_regular_file "$node_dir/compute-model.json" "compute model fixture"
    model_choice_sha256="$(sha "$node_dir/compute-model.json")"
    expected_model_path="$(jq -er '.path' "$node_dir/compute-model.json")"
    assert_regular_file "$expected_model_path" "verified model fixture"
    started=$(date +%s)
    sudo systemctl start arc-node
    wait_health http://127.0.0.1:9944 600 || fail "the previous opted-in worker did not become healthy"
    printf '%s\n' "$(( $(date +%s) - started ))" > "$evidence/prior-compute-start-seconds.txt"
    assert_running_node "$prior_node_tag" "$prior_node_version" "$prior_node_sha256" "$prior_address" \
        "$evidence/node-info.prior-compute.json" "$evidence/prior-compute.argv"
    grep -qx -- --full-integer-worker "$evidence/prior-compute.argv" || fail "the previous node was not an opted-in worker"
    [ "$(argv_value_in "$evidence/prior-compute.argv" --model)" = "$expected_model_path" ] \
        || fail "the previous worker did not receive the verified model path"
    [ "$node_tag" != "$prior_node_tag" ] || fail "candidate node tag must differ from prior node tag"
    rm -rf "${arc_dir:?}/legacy-bridge/releases/${node_tag:?}"
    assert_absent_path "$arc_dir/legacy-bridge/releases/$node_tag" "cold-cache clear before compute upgrade"
    assert_regular_file "$arc_dir/legacy-bridge/releases/$prior_node_tag/$asset" "prior node payload before compute upgrade"
    assert_regular_file "$expected_model_path" "verified model before compute upgrade"
    started=$(date +%s)
    sudo systemctl start arc-updater.service
    printf '%s\n' "$(( $(date +%s) - started ))" > "$evidence/compute-update-seconds.txt"
    cp "$arc_dir/auto-update.log" "$evidence/auto-update.compute.log"
    [ "$(cat "$arc_dir/version.txt")" = "$bridge_version" ] || fail "the real 30-second updater rolled back the opted-in worker"
    [ "$(sha "$arc_dir/bin/arc-node")" = "$bridge_sha" ] || fail "bin/arc-node is not the candidate bridge after compute update"
    local_curl 5 http://127.0.0.1:9944/health > "$evidence/health.compute.after-updater.json"
    python3 "$here/worker_startup_readiness.py" \
        --model-id "$(jq -er '.shard_worker.expected_model_id' "$pins")" \
        --profile 'INT8 integer (per-row, cross-platform deterministic)' \
        --out "$evidence/compute-readiness.jsonl"
    assert_running_node "$node_tag" "$node_version" "$node_sha256" "$prior_address" \
        "$evidence/node-info.compute.json" "$evidence/updated-compute.argv"
    grep -qx -- --full-integer-worker "$evidence/updated-compute.argv" || fail "the update removed the worker role"
    [ "$(argv_value_in "$evidence/updated-compute.argv" --model)" = "$expected_model_path" ] \
        || fail "the update did not preserve the verified model path"
    assert_regular_file "$expected_model_path" "verified model after compute update"
    assert_regular_file "$node_dir/compute-consent" "compute opt-in after compute update"
    [ "$(cat "$node_dir/compute-consent")" = yes ] \
        || fail "the update discarded the compute opt-in"
    assert_regular_file "$node_dir/compute-worker-mode" "explicit worker mode after compute update"
    [ "$(cat "$node_dir/compute-worker-mode")" = full ] \
        || fail "the update discarded the explicit full-worker override"
    assert_regular_file "$node_dir/compute-model.json" "chosen model after compute update"
    [ "$(sha "$node_dir/compute-model.json")" = "$model_choice_sha256" ] \
        || fail "the update changed the chosen model"
    assert_regular_file "$prior_keyfile" "private key after compute update"
    [ "$(sha "$prior_keyfile")" = "$prior_key_sha256" ] \
        || fail "the opted-in update changed the private key"
    assert_regular_file "$node_data/genesis.network-hash" "genesis binding after compute update"
    [ "$(sha "$node_data/genesis.network-hash")" = "$prior_genesis_sha256" ] \
        || fail "the opted-in update changed the persisted genesis binding"
    assert_wal_prefix_preserved "$evidence/prior-bridge-state.wal" "$node_data/state.wal" "opted-in worker update"
    assert_regular_file "$arc_dir/legacy-bridge/preserved/arc-node.before-bridge" "original rollback binary after compute update"
    [ "$(sha "$arc_dir/legacy-bridge/preserved/arc-node.before-bridge")" = "$legacy_node_sha256" ] \
        || fail "the opted-in update replaced the original rollback binary"
    sudo systemctl stop arc-node

    log "An existing consenting worker without a mode preference automatically qualifies a partial shard"
    rm -- "$node_dir/compute-worker-mode"
    assert_absent_path "$node_dir/compute-worker-mode" "automatic-mode fixture setup"
    install -m 0755 "$state/releases/$prior_tag/$asset" "$arc_dir/bin/arc-node"
    printf '%s\n' "$prior_version" > "$arc_dir/version.txt"
    sudo systemctl start arc-node
    wait_health http://127.0.0.1:9944 600 || fail "the previous opted-in worker did not restart for automatic-mode upgrade"
    assert_running_node "$prior_node_tag" "$prior_node_version" "$prior_node_sha256" "$prior_address" \
        "$evidence/node-info.prior-auto-compute.json" "$evidence/prior-auto-compute.argv"
    grep -qx -- --full-integer-worker "$evidence/prior-auto-compute.argv" || fail "the previous bridge fixture did not retain whole-model compute"
    rm -rf "${arc_dir:?}/legacy-bridge/releases/${node_tag:?}"
    assert_absent_path "$arc_dir/legacy-bridge/releases/$node_tag" "cold-cache clear before automatic shard upgrade"
    assert_regular_file "$arc_dir/legacy-bridge/releases/$prior_node_tag/$asset" "prior node payload before automatic shard upgrade"
    assert_regular_file "$expected_model_path" "verified model before automatic shard upgrade"
    sudo systemctl start arc-updater.service
    cp "$arc_dir/auto-update.log" "$evidence/auto-update.auto-shard.log"
    [ "$(cat "$arc_dir/version.txt")" = "$bridge_version" ] || fail "the real 30-second updater rolled back the automatic shard worker"
    [ "$(sha "$arc_dir/bin/arc-node")" = "$bridge_sha" ] || fail "automatic-mode update did not retain the candidate launcher"
    wait_node_info "$evidence/node-info.auto-shard.startup.json" "$node_version" "$prior_address" 600 \
        || fail "the automatic shard worker did not complete local startup"
    assert_running_node "$node_tag" "$node_version" "$node_sha256" "$prior_address" \
        "$evidence/node-info.auto-shard.json" "$evidence/updated-auto-shard.argv"
    grep -qx -- --community-shard-worker "$evidence/updated-auto-shard.argv" || fail "eligible automatic-mode upgrade did not select partial shards"
    for forbidden in --full-integer-worker --community-mode --community-rpc-url --no-community; do
        if grep -qx -- "$forbidden" "$evidence/updated-auto-shard.argv"; then
            fail "automatic shard worker passed forbidden flag $forbidden"
        fi
    done
    [ "$(argv_value_in "$evidence/updated-auto-shard.argv" --threads)" = 1 ] \
        || fail "automatic shard worker did not pin --threads 1"
    shard_budget="$(argv_value_in "$evidence/updated-auto-shard.argv" --community-shard-max-working-set-bytes)"
    [[ "$shard_budget" =~ ^[1-9][0-9]*$ ]] \
        || fail "automatic shard worker has invalid --community-shard-max-working-set-bytes: $shard_budget"
    [ "$(argv_value_in "$evidence/updated-auto-shard.argv" --model)" = "$expected_model_path" ] \
        || fail "automatic shard upgrade changed the verified model path"
    assert_regular_file "$expected_model_path" "verified model after automatic shard upgrade"
    for binding in coordinator-url:coordinator_url coordinator-id:coordinator_id transaction-domain:transaction_domain expected-model-id:expected_model_id; do
        [ "$(argv_value_in "$evidence/updated-auto-shard.argv" "--community-shard-${binding%%:*}")" = "$(jq -er --arg field "${binding#*:}" '.shard_worker[$field]' "$pins")" ] \
            || fail "automatic shard worker has the wrong $binding pin"
    done
    assert_absent_path "$node_dir/compute-worker-mode" "automatic shard upgrade mode file"
    assert_regular_file "$node_dir/compute-consent" "compute consent after automatic shard upgrade"
    [ "$(cat "$node_dir/compute-consent")" = yes ] \
        || fail "automatic shard upgrade discarded compute consent"
    assert_regular_file "$node_dir/compute-model.json" "chosen model after automatic shard upgrade"
    [ "$(sha "$node_dir/compute-model.json")" = "$model_choice_sha256" ] \
        || fail "automatic shard upgrade changed the chosen model"
    assert_regular_file "$prior_keyfile" "private key after automatic shard upgrade"
    [ "$(sha "$prior_keyfile")" = "$prior_key_sha256" ] \
        || fail "automatic shard upgrade changed the private key"
    assert_regular_file "$node_data/genesis.network-hash" "genesis binding after automatic shard upgrade"
    [ "$(sha "$node_data/genesis.network-hash")" = "$prior_genesis_sha256" ] \
        || fail "automatic shard upgrade changed the genesis binding"
    assert_wal_prefix_preserved "$evidence/prior-bridge-state.wal" "$node_data/state.wal" "automatic shard worker update"
    assert_regular_file "$arc_dir/legacy-bridge/preserved/arc-node.before-bridge" "original rollback binary after automatic shard upgrade"
    [ "$(sha "$arc_dir/legacy-bridge/preserved/arc-node.before-bridge")" = "$legacy_node_sha256" ] \
        || fail "automatic shard upgrade replaced the original rollback binary"
    local_curl 5 http://127.0.0.1:9944/inference/readiness > "$evidence/auto-shard-isolated-readiness.json"
    jq -e --arg profile 'INT8 integer (per-row, cross-platform deterministic)' \
        '.schema == "arc.inference.readiness.v1" and .safe_to_dispatch == false and .community_dispatch_ready == false and .local_model_ready == false and .sharded_pipeline_ready == false and .live_community_workers == 0 and .model_id == null and .required_community_execution_profile == $profile and .mutation_free_observation == true' \
        "$evidence/auto-shard-isolated-readiness.json" >/dev/null \
        || fail "an isolated partial worker returned an invalid or falsely ready inference readiness snapshot"
    sudo systemctl stop arc-node
fi

log "PASS: v0.7.11 headless install bridged to $node_tag at stake 0; v0.7 data untouched"

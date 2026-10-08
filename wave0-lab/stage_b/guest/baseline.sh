#!/usr/bin/env bash
# Wave 0 baseline: the stranded v0.7.11 headless install, built exactly as
# tests/legacy-bridge/headless-v07-acceptance.sh builds it (sections "Collect the released v0.7 sources and the
# pinned v0.7.7 binary", "Instrument (never modify) the v0.7 units" and "Install exactly as a v0.7 operator did"),
# with these deliberate differences, each for one reason:
#   * the released v0.7.11 sources come from /opt/arc-w0/legacy-source (the host ran `git show v0.7.11:...`);
#   * no live-network isolation here: the arc-w0-live-block unit owns it, so it survives a guest reboot;
#   * NO fake-github drop-in on arc-updater.service and the updater timer stays running: Wave 0 uses the REAL
#     updater against the real GitHub API, as the field does;
#   * the bridge is not published through the fake Latest: it is consumed later by canary-consume.sh.
# THROWAWAY LAB FILE. Runs as the unprivileged guest user with passwordless sudo. Prints what it did.
set -Eeuo pipefail

here=/opt/arc-w0/tests/legacy-bridge
pins=/opt/arc-w0/crates/arc-legacy-bridge/pins/active.json
src=/opt/arc-w0/legacy-source
work=/var/lib/arc-w0
evidence="$work/baseline"
snapshots="$work/snapshots"

legacy_tag=v0.7.11
legacy_node_tag=v0.7.7
legacy_node_sha256=1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c
asset=arc-node-linux-x86_64

bridge_version="$(jq -er '.bridge_version' "$pins")"
bridge_tag="v$bridge_version"

export USER="${USER:-$(id -un)}"
arc_dir="$HOME/.arc"
state="$evidence/fake-github"
shim_bin="$here/fake-github"
mkdir -p "$evidence" "$state/api" "$state/raw/main" "$snapshots"
: > "$state/requests.log"

log() { printf '\n== %s\n' "$*"; }
fail() { printf 'BASELINE FAILURE: %s\n' "$*" >&2; exit 1; }
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

log "Collect the released v0.7 sources and the pinned v0.7.7 binary"
legacy="$evidence/legacy-source"
mkdir -p "$legacy"
cp "$src/install-community-node.sh" "$legacy/install-community-node.sh"
cp "$src/testnet-seeds.txt" "$state/raw/main/testnet-seeds.txt"
cp "$src/genesis.toml" "$state/raw/main/genesis.toml"
mkdir -p "$state/releases/$legacy_node_tag" "$state/releases/$bridge_tag"
/usr/bin/curl -fsSL --retry 3 -o "$state/releases/$legacy_node_tag/$asset" \
    "https://github.com/FerrumVir/arc-chain/releases/download/$legacy_node_tag/$asset"
[ "$(sha "$state/releases/$legacy_node_tag/$asset")" = "$legacy_node_sha256" ] \
    || fail "the v0.7.7 binary does not match its pinned digest"
/usr/bin/curl -fsS --retry 3 -o "$evidence/real-releases-latest.json" \
    "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest"
printf 'live GitHub Latest today: %s\n' "$(jq -r .tag_name "$evidence/real-releases-latest.json")"

log "Instrument (never modify) the v0.7 units the installer will write"
sudo mkdir -p /etc/systemd/system/arc-node.service.d
sudo tee /etc/systemd/system/arc-node.service.d/50-acceptance-snapshot.conf >/dev/null <<EOT
[Service]
ExecStartPre=/usr/bin/python3 $here/snapshot_tree.py hook --root $arc_dir/data --binary $arc_dir/bin/arc-node --out-dir $snapshots
EOT

log "Install exactly as a v0.7 operator did: the unmodified v0.7.11 installer"
publish_latest "$legacy_node_tag"
export ARC_FAKE_GITHUB_STATE="$state"
PATH="$shim_bin:$PATH" bash "$legacy/install-community-node.sh" > "$evidence/v07-install.log" 2>&1 \
    || { cat "$evidence/v07-install.log"; fail "the v0.7.11 installer failed"; }
grep -q 'Latest version: v0.7.7' "$evidence/v07-install.log"
[ "$(cat "$arc_dir/version.txt")" = 0.7.7 ] || fail "v0.7 version.txt is not 0.7.7"
[ "$(sha "$arc_dir/bin/arc-node")" = "$legacy_node_sha256" ] || fail "installed binary is not v0.7.7"
grep -q -- '--stake 0 --min-stake 0' /etc/systemd/system/arc-node.service
grep -q -- '--data-dir '"$arc_dir"'/data' /etc/systemd/system/arc-node.service
wait_health http://127.0.0.1:9944 60 || fail "the v0.7.7 node never answered /health"
sleep 15
[ -s "$arc_dir/data/state.wal" ] || fail "the v0.7.7 node wrote no state WAL"
v07_pid="$(main_pid)"
[ "$(readlink "/proc/$v07_pid/exe")" = "$arc_dir/bin/arc-node" ] || fail "v0.7 is not the running node"
seed="$(cat "$arc_dir/identity.seed")"
[ -n "$seed" ] || fail "the v0.7 installer created no identity seed"
systemctl is-enabled arc-updater.timer | grep -qx enabled || fail "the v0.7 updater timer is not enabled"
systemctl is-active arc-updater.timer | grep -qx active || fail "the v0.7 updater timer is not active"

log "PASS: stranded v0.7.11 headless install ready (v0.7.7 node pid $v07_pid, real updater timer active)"
printf 'installer=%s legacy_node=%s bridge_tag=%s legacy_node_sha256=%s v07_pid=%s\n' "$legacy_tag" "$legacy_node_tag" "$bridge_tag" "$legacy_node_sha256" "$v07_pid" > "$evidence/baseline-result.txt"

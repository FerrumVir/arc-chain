#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"
ORCHESTRATOR="$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh"
NODE_HELPER="$REPO_ROOT/scripts/recovery/archive-node.sh"
ROLLOUT="$REPO_ROOT/scripts/recovery/recovery_rollout.py"
SCHEMA="$REPO_ROOT/scripts/recovery/recovery-manifest.schema.json"
FREEZE_MODULE="$REPO_ROOT/scripts/recovery/recovery_freeze.py"
FREEZE_MODULE_TEST="$REPO_ROOT/scripts/recovery/test_recovery_freeze.py"
SIGNAL_PROBE="$REPO_ROOT/tests/release/fixtures/archive_dispatch_signal_probe.sh"

exact_authorizations_bind_every_domain() {
    for required in 'expected_go="STAGE-BARRIERS $orchestrator_sha HELPER $helper_sha"' \
      'expected_go="FREEZE $freeze_sha CAPTURE $capture_id"' \
      'expected_go="GO $manifest_sha FREEZE $freeze_sha CAPTURE $capture_id DEST $destination_sha LEGACY_WAL $policy"' \
      'ARCHIVE {archive_manifest_sha256}' 'DEST {destination_sha} LEGACY_WAL {policy}' \
      remote_helper_sha256 rollout_tool_sha256 rollout_schema_sha256; do
        grep -Fq -- "$required" "$ORCHESTRATOR" "$ROLLOUT" || return 1
    done
    PYTHONPATH="${ROLLOUT%/*}" python3 - <<'PY' || return 1
import hashlib, recovery_rollout as rr
freeze="ab"*32
assert rr.capture_id_for_freeze_plan_hash(freeze)==hashlib.sha256(b"ARC recovery capture v2\0"+bytes.fromhex(freeze)).hexdigest()
PY
    python3 - "$NODE_HELPER" <<'PY' || return 1
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
body=text[text.index("def validate_authorization(raw):"):text.index("def validate_acceptance(")]
for field in (
    "live_observation_selection_sha256", "live_observation_generation",
    "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
    "live_observation_selected_at",
):
    assert field in body, field
assert 'HASH_RE.fullmatch(str(value.get(field))) is None' in body
assert 'deadline > public_completed + datetime.timedelta(seconds=300)' not in body
assert 'deadline > observation_selected_at + datetime.timedelta(seconds=300)' in body
assert 'authorized < observation_selected_at' in body
assert 'CLOCK_BOOTTIME' in text and 'accepted_monotonic_ns' in text
PY
}

capture_id_and_destination_fail_closed() {
    PYTHONPATH="${ROLLOUT%/*}" python3 - <<'PY' || return 1
import recovery_rollout as rr
freeze="e"*64; capture=rr.capture_id_for_freeze_plan_hash(freeze)
rr.validate_drive_remote(f"arc-drive:ARC Chain Recovery/captures/{capture}","destination")
for bad in ("-drive:path","arc-drive:../escape","arc-drive:path/../escape","arc-drive:path\nmore"):
    try: rr.validate_drive_remote(bad,"destination")
    except rr.RolloutError: pass
    else: raise AssertionError(bad)
PY
}

timer_units_with_empty_mainpid_normalize_to_zero() {
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
audit = text[text.index("audit_writers()") : text.index("seal_freeze_plan()")]
expected = '"main_pid": int(prepare_prop("MainPID") or "0"),'
assert expected in audit
assert '"main_pid": int(prepare_prop("MainPID")),' not in audit

# Ubuntu systemd emits an empty MainPID for timer units.  Keep the exact
# production normalization executable in this regression, not just documented.
prepare_prop = lambda _name: ""
assert int(prepare_prop("MainPID") or "0") == 0
PY
}

sealed_stake_quorum_never_claims_global_halt() {
    for required in 'source_total_stake") != 40_000_000' controlled_writer_stake \
      controlled_quorum_unavailable_after_all_stops global_legacy_halt_claimed \
      dynamic_membership_disagrees untrusted_external_observations; do
        grep -Fq -- "$required" "$ORCHESTRATOR" || return 1
    done
    python3 - <<'PY' || return 1
source,controlled=40_000_000,30_000_000; quorum=source*2//3+1
assert controlled*3>source and source-controlled<quorum
assert len({35_000_000,40_000_000,50_000_000})>1
PY
    ! grep -Fq 'QUORUM HALTED' "$ORCHESTRATOR"
}

all_six_exact_writers_stop_before_content_capture() {
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
t=pathlib.Path(sys.argv[1]).read_text(); b=t[t.index("capture_phase()"):t.index("manifest_field()")]
assert b.index('run_drive_prefreeze_gate execute') < b.index('capture_all_live_observations')
late_sample=b.index('legacy_height_receipt_sha="$(sample_legacy_public_height_late')
height_cross=b.index('capture_authenticated_legacy_height_cross_proof "$freeze_plan"')
fresh_capacity=b.index('remote_readiness "$capture_id" "$freeze_sha" "$freeze_plan"')
rounds=b.index('run_quarantine_generation_rounds')
first_boundary=b.index('build-first-boundary')
stop=b.index('stop_after_quarantine_round_exact')
capture=b.index('ensure_offline_capture "$capture_id" "$node"')
persisted=b.index('run_persisted_head_exact "$freeze_plan"')
boundary=b.index('create_legacy_maintenance_boundary')
offline=b.index('create_offline_stop_evidence')
assert b.index('capture_all_live_observations') < late_sample < height_cross < fresh_capacity < rounds < first_boundary
assert ('remote_readiness "$capture_id" "$freeze_sha" "$freeze_plan"\n'
        '    quarantine_generation_ledger_sha="$(run_quarantine_generation_rounds \\' in b)
assert first_boundary < stop < capture < persisted < boundary < offline
assert 'ALL SIX CONTROLLED WRITERS HALTED' in b and 'no global halt is claimed' in b
for required in ('sample-targets', 'quarantine-round-authorize',
                 'quarantine-round-ready', 'quarantine-round-apply',
                 'quarantine-round-stopped-precommit'):
    assert required in t, required
for obsolete in ('run_quarantine_exact', 'run_quarantine_starter_exact',
                 'create_quarantine_fleet_start_readiness', 'ensure_stopped'):
    assert obsolete not in t, obsolete
PY
    ! grep -Eq 'pkill[[:space:]]|killall[[:space:]]|kill[[:space:]]+-9' "$NODE_HELPER"
}

late_public_height_sampling_is_plan_safe_and_create_only() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local fixture output first_sha first_raw pinned status
    fixture="$(mktemp -d "$REPO_ROOT/.late-height-test.XXXXXX")"
    trap 'chmod -R u+w "$fixture" 2>/dev/null || true; rm -rf -- "$fixture"' EXIT
    chmod 700 "$fixture"
    mkdir "$fixture/source"
    python3 - "$fixture/source/legacy-public-height.py" <<'PY' || return 1
import pathlib,sys
path=pathlib.Path(sys.argv[1])
path.write_text('''#!/usr/bin/env python3
import hashlib,json,os,pathlib,sys
output=pathlib.Path(sys.argv[sys.argv.index("--output")+1])
payload=b'{"sampled_after_prerequisites":true}\\n'
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:
    handle.write(payload);handle.flush();os.fsync(handle.fileno())
print(json.dumps({"legacy_public_max_height":425,"receipt_sha256":hashlib.sha256(payload).hexdigest()},sort_keys=True,separators=(",",":")))
''',encoding="utf-8")
(path.parent/"recovery_freeze.py").write_text("# pinned dependency\\n",encoding="utf-8")
(path.parent/"quarantine_rounds.py").write_text("# pinned dependency\\n",encoding="utf-8")
PY
    LEGACY_HEIGHT_TOOL="$fixture/source/legacy-public-height.py"
    RECOVERY_FREEZE_MODULE="$fixture/source/recovery_freeze.py"
    QUARANTINE_ROUND_MODULE="$fixture/source/quarantine_rounds.py"
    [ -f "$LEGACY_HEIGHT_TOOL" ] || return 1
    [ -f "$RECOVERY_FREEZE_MODULE" ] || return 1
    [ -f "$QUARANTINE_ROUND_MODULE" ] || return 1
    tracked_source_hash() { hash_file "$1"; }
    manifest_field() { printf '%s\n' "$(printf '1%.0s' {1..40})"; }
    pinned="$(pin_legacy_public_height_toolchain "$fixture/pinned")" || return 1
    [ "$pinned" = "$fixture/pinned/legacy-public-height.py" ] || return 1
    python3 - "$fixture/pinned" <<'PY' || return 1
import pathlib,stat,sys
root=pathlib.Path(sys.argv[1])
assert stat.S_IMODE(root.stat().st_mode)==0o700
for name in ("legacy-public-height.py","recovery_freeze.py","quarantine_rounds.py"):
    assert stat.S_IMODE((root/name).stat().st_mode)==0o400
PY
    [ ! -e "$fixture/pinned/__pycache__" ] || return 1
    LEGACY_HEIGHT_TOOL="$pinned"
    [ "$LEGACY_HEIGHT_TOOL" = "$pinned" ] || return 1
    output="$fixture/late-height.json"
    [ "$(validate_legacy_public_height_sample_output "$output")" = absent ] || return 1
    [ ! -e "$output" ] || return 1
    first_sha="$(sample_legacy_public_height_late \
        "$fixture/freeze.json" "$(printf '2%.0s' {1..64})" "$output")" || return 1
    [ "$first_sha" = "$(hash_file "$output")" ] || return 1
    [ "$(validate_legacy_public_height_sample_output "$output")" = sealed ] || return 1
    first_raw="$(base64 < "$output")"
    if ( trap - EXIT; sample_legacy_public_height_late \
            "$fixture/freeze.json" "$(printf '2%.0s' {1..64})" "$output" \
            >/dev/null 2>&1 ); then
        return 1
    else
        status=$?
    fi
    [ "$status" -ne 0 ] || return 1
    [ "$(base64 < "$output")" = "$first_raw" ] || return 1

    chmod 600 "$output"
    if ( trap - EXIT; validate_legacy_public_height_sample_output "$output" \
            >/dev/null 2>&1 ); then return 1; fi
    chmod 400 "$output"
    ln "$output" "$fixture/hardlink.json"
    if ( trap - EXIT; validate_legacy_public_height_sample_output "$output" \
            >/dev/null 2>&1 ); then return 1; fi
    rm "$fixture/hardlink.json"
    ln -s "$output" "$fixture/symlink.json"
    if ( trap - EXIT; validate_legacy_public_height_sample_output \
            "$fixture/symlink.json" >/dev/null 2>&1 ); then return 1; fi
    mkdir -m 777 "$fixture/writable"
    if ( trap - EXIT; validate_legacy_public_height_sample_output \
            "$fixture/writable/new.json" >/dev/null 2>&1 ); then return 1; fi

    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
capture=text[text.index("capture_phase()"):text.index("manifest_field()")]
plan_return=capture.index("if [ \"$execute\" != true ]")
late_sample=capture.index('legacy_height_receipt_sha="$(sample_legacy_public_height_late')
live=capture.index("capture_all_live_observations")
cross=capture.index('capture_authenticated_legacy_height_cross_proof "$freeze_plan"')
assert plan_return < live < late_sample < cross
assert "--sample-legacy-public-height-output" in capture
assert "mutually exclusive with an existing receipt/hash" in capture
assert "unselected late legacy public-height receipt exists" in capture
assert "collides with the offline-stop evidence namespace" in capture
assert "capture-scoped with a unique 32-hex nonce" in capture
PY
)

freeze_plan_install_reuse_is_root_safe_and_crash_resumable() (
    local fixture target sidecar payload_sha
    fixture="$(mktemp -d "$REPO_ROOT/.freeze-plan-reuse-test.XXXXXX")"
    trap 'chmod -R u+w "$fixture" 2>/dev/null || true; rm -rf -- "$fixture"' EXIT
    chmod 700 "$fixture"
    target="$fixture/freeze.lock.json"
    sidecar="$target.sha256"
    printf '%s\n' '{"sealed":true}' > "$target"
    payload_sha="$(python3 - "$target" <<'PY'
import hashlib,pathlib,sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)"
    printf '%s  %s\n' "$payload_sha" "${target##*/}" > "$sidecar"
    chmod 400 "$target" "$sidecar"
    # Model an interrupted hard-link publication: the immutable inode is
    # correct, but cleanup did not yet remove the uploader-side link.
    ln "$target" "$fixture/retained-upload"
    ln "$sidecar" "$fixture/retained-sidecar-partial"
    python3 - "$ORCHESTRATOR" "$target" "$sidecar" "$payload_sha" <<'PY' || return 1
import hashlib
import os
import pathlib
import stat
import sys

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
target = pathlib.Path(sys.argv[2])
sidecar = pathlib.Path(sys.argv[3])
expected = sys.argv[4]
install = source[source.index("install_freeze_plan()") : source.index("prepare_writers()")]

assert 'test ! -w "$target"' not in install
assert 'test ! -w "$sidecar"' not in install
assert '$(/usr/bin/stat -c %u:%g:%a -- "$target")" = 0:0:400' in install
assert '$(/usr/bin/stat -c %u:%g:%a -- "$sidecar")" = 0:0:400' in install
assert "%h" not in install

def identity(path):
    details = path.lstat()
    return {
        "regular": stat.S_ISREG(details.st_mode),
        "symlink": stat.S_ISLNK(details.st_mode),
        # The remote command is authenticated as root; normalize this local
        # fixture to the exact remote numeric identity contract.
        "uid": 0,
        "gid": 0,
        "mode": stat.S_IMODE(details.st_mode),
        "nlink": details.st_nlink,
    }

def accepted(details):
    return (
        details["regular"]
        and not details["symlink"]
        and (details["uid"], details["gid"], details["mode"]) == (0, 0, 0o400)
    )

target_identity = identity(target)
sidecar_identity = identity(sidecar)
assert target_identity["nlink"] == 2 and sidecar_identity["nlink"] == 2
assert accepted(target_identity) and accepted(sidecar_identity)
assert hashlib.sha256(target.read_bytes()).hexdigest() == expected
assert sidecar.read_bytes() == f"{expected}  {target.name}\n".encode("ascii")
for field, bad in (("uid", 1), ("gid", 1), ("mode", 0o600),
                   ("regular", False), ("symlink", True)):
    altered = dict(target_identity); altered[field] = bad
    assert not accepted(altered), field
# Link count is deliberately outside the reuse predicate: both the ordinary
# nlink=1 state and an interrupted nlink=2 publication are recoverable.
ordinary = dict(target_identity); ordinary["nlink"] = 1
assert accepted(ordinary) and accepted(target_identity)
assert hashlib.sha256(target.read_bytes() + b"tamper").hexdigest() != expected
assert sidecar.read_bytes() + b"tamper" != f"{expected}  {target.name}\n".encode("ascii")
PY
)

offline_stop_roots_are_remote_derived_and_archive_bound() {
    python3 - "$ORCHESTRATOR" "$NODE_HELPER" <<'PY' || return 1
import pathlib, re, sys
fleet = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
helper = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")
text = fleet
capture = fleet[fleet.index("capture_phase()"):fleet.index("manifest_field()")]
seal = fleet[fleet.index("seal_phase()") :]
assert "arc.recovery.offline-stop-status.v1" in helper
assert "arc.recovery.offline-stop.v4" in helper
assert "stop_complete_sha256" in helper and "stop_files_sha256" in helper
assert "arc.validator-vault.offline-stop-evidence.v2" in fleet
assert "stopped_status_argv_sha256" in fleet and "stopped_status_sha256" in fleet
assert capture.index("ensure_offline_capture") < capture.index("run_persisted_head_exact")
assert capture.index("run_persisted_head_exact") < capture.index("create_legacy_maintenance_boundary")
assert capture.index("create_legacy_maintenance_boundary") < capture.index("create_offline_stop_evidence")
assert seal.index("verify_offline_stop_evidence_remote") < seal.index("PLAN ONLY")
assert 'offline-stop-evidence.json' in seal
for name in (
    'offline-stop-evidence.json.sha256',
    'legacy-maintenance-evidence-bundle.json',
    'legacy-maintenance-evidence-bundle.json.sha256',
    'legacy-maintenance-boundary.json',
    'legacy-maintenance-boundary.json.sha256',
    'legacy-late-fork-source-set.json',
    'legacy-late-fork-source-set.json.sha256',
    'legacy-late-fork-interlock.py',
    'drive-archive-seal-attempt.json',
):
    assert name in seal, name
assert 'arc.recovery.drive-archive-seal-attempt.v1' in text
for function in ('reserve_stop_boundary_timestamp', 'publish_canonical_maintenance_input',
                 'reserve_quarantine_challenge', 'create_legacy_maintenance_boundary',
                 'create_offline_stop_evidence', 'seal_archive_finalization_intent',
                 'write_gist_anchor_receipt'):
    start=text.index(function+'()')
    search_from=text.index('\n',start)+1
    next_function=re.search(r'(?m)^[_a-zA-Z][_a-zA-Z0-9]*\(\) [({]\s*$',text[search_from:])
    end=(search_from+next_function.start()) if next_function else len(text)
    body=text[start:end]
    assert '.partial' in body, function
    if function in ('reserve_stop_boundary_timestamp', 'publish_canonical_maintenance_input'):
        assert 'os.link' in body and 'os.rename' not in body, function
    else:
        assert 'os.rename' in body or 'os.link' in body, function
PY
}

offline_stop_receipt_is_canonical_private_and_adversarial() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f freeze_sha capture receipt_sha
    f="$(mktemp -d "$REPO_ROOT/.offline-stop-test.XXXXXX")"
    trap 'chmod -R u+w "$f" 2>/dev/null || true; rm -rf -- "$f"' EXIT
    mkdir -p "$f/status"
    python3 - "$f" <<'PY' || return 1
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
fleet = (
    ("nyc", "149.28.32.76"), ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"), ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"), ("sgp", "149.28.153.31"),
)

canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
nodes = []
for index, (name, host) in enumerate(fleet):
    nodes.append({
        "name": name, "host": host, "validator_address": f"{index + 1:064x}",
        "stake": 5_000_000, "writer_pid": 100 + index,
        "writer_start_ticks": 200 + index, "boot_id": f"00000000-0000-0000-0000-{index + 1:012x}",
        "writer_cgroup_sha256": "1" * 64, "writer_supervision_mode": "systemd-unit",
        "supervisor_unit": "arc-node.service", "supervisor_main_pid": 100 + index,
        "supervisor_start_ticks": 200 + index, "supervisor_executable_path": "/usr/bin/systemd",
        "supervisor_executable_sha256": "2" * 64, "supervisor_argv_sha256": "3" * 64,
        "supervisor_context_sha256": "4" * 64, "executable_path": "/usr/local/bin/arc-node",
        "executable_sha256": "5" * 64, "argv_sha256": "6" * 64,
        "data_dir": "/var/lib/arc-data", "rpc_origin": "http://127.0.0.1:9090",
    })
plan = {"schema": "arc.recovery.freeze-plan.v5", "source_commit": "7" * 40,
        "remote_helper_sha256": "8" * 64, "nodes": nodes}
payload = canonical(plan); freeze_sha = hashlib.sha256(payload).hexdigest()
(root / "freeze.json").write_bytes(payload)
(root / "freeze.json.sha256").write_text(f"{freeze_sha}  freeze.json\n", encoding="ascii")
capture = hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(freeze_sha)).hexdigest()
for index, node in enumerate(nodes):
    status = {"schema": "arc.recovery.offline-stop-status.v1", "capture_id": capture,
              "node": node["name"], "freeze_plan_sha256": freeze_sha,
              "validator_address": node["validator_address"], "stake": node["stake"],
              "stopped": True, "restart_fenced": True,
              "stop_schema": "arc.recovery.offline-stop.v4",
              "stop_complete_sha256": f"{index + 20:064x}",
              "stop_files_sha256": f"{index + 40:064x}"}
    (root / "status" / f"{node['name']}-stopped-status.json").write_bytes(canonical(status))
cross_rows = []
for index, (name, host) in enumerate(fleet):
    proof = {"schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
             "node": name, "conservative_height_floor": 100 + index}
    cross_rows.append({"node": name, "host": host, "proof": proof,
                       "proof_sha256": hashlib.sha256(canonical(proof)).hexdigest()})
cross = {"schema": "arc.recovery.authenticated-legacy-height-fleet.v1",
         "source_main_commit": "7" * 40, "freeze_plan_sha256": freeze_sha,
         "capture_id": capture, "conservative_height_floor": 105,
         "nodes": cross_rows}
(root / "legacy-height-cross-proof.json").write_bytes(canonical(cross))
boundary = {
    "schema":"arc.recovery.legacy-maintenance-boundary.v1",
    "source_main_commit":"7"*40,"freeze_plan_sha256":freeze_sha,"capture_id":capture,
    "first_quarantine_started_at":"2026-08-28T12:00:00Z",
    "all_controlled_stopped_at":"2026-08-28T12:00:01Z","created_at":"2026-08-28T12:00:02Z",
    "official_origin_scope":{"global_absence_claimed":False,"origins":[]},
    "legacy_public_height_receipt":{},"authenticated_prefence_height_cross_proof_sha256":"9"*64,
    "quarantine_generation_ledger_sha256":"c"*64,
    "legacy_maintenance_evidence_bundle_sha256":"0"*64,
    "legacy_live_observation_selection_sha256":"0"*64,
    "legacy_live_observation_generation":"d"*64,
    "observation_generation_receipt_sha256":"e"*64,
    "drive_prefreeze_receipt_sha256":"f"*64,
    "network_quarantine_challenge":"a"*64,
    "network_quarantine_stability_proof_sha256":"b"*64,"tools":{},
    "nodes":[{"node":name,"host":host} for name,host in fleet],"evidence_heights":[],
    "observed_cutoff_height":105,"continuity_safety_margin":128,
    "continuity_safety_margin_policy":{},"legacy_public_max_height":233,
    "global_absence_claimed":False,"reopening_policy":{},"late_fork_circuit":{},"threat_model":{},
}
selection={"schema":"arc.recovery.legacy-live-observation-selection.v1",
           "observation_generation":"d"*64,
           "observation_generation_receipt_sha256":"e"*64,
           "drive_prefreeze_receipt_sha256":"f"*64}
selection_sha=hashlib.sha256(canonical(selection)).hexdigest()
boundary["legacy_live_observation_selection_sha256"]=selection_sha
bundle={"schema":"arc.recovery.legacy-maintenance-evidence-bundle.v1",
        "source_main_commit":"7"*40,"freeze_plan_sha256":freeze_sha,"capture_id":capture,
        "first_quarantine_started_at":"2026-08-28T12:00:00Z",
        "all_controlled_stopped_at":"2026-08-28T12:00:01Z","challenge":"a"*64,
        "authenticated_prefence_height_cross_proof":{},
        "live_observation_selection":{"value":selection,"sha256":selection_sha},
        "quarantine_generation_ledger":{"value":{},"sha256":"c"*64},
        "network_quarantine_challenge":{},
        "quarantine_stability_proof":{"value":{},"sha256":"b"*64},
        "nodes":[{"node":name,"host":host} for name,host in fleet],
        "object_inventory":[],"aggregate_root_sha256":"1"*64}
bundle_raw=canonical(bundle);bundle_sha=hashlib.sha256(bundle_raw).hexdigest()
boundary["legacy_maintenance_evidence_bundle_sha256"]=bundle_sha
(root/"legacy-maintenance-evidence-bundle.json").write_bytes(bundle_raw)
(root/"legacy-maintenance-evidence-bundle.json.sha256").write_text(
    f"{bundle_sha}  legacy-maintenance-evidence-bundle.json\n",encoding="ascii")
boundary_raw=canonical(boundary);(root/"legacy-maintenance-boundary.json").write_bytes(boundary_raw)
(root/"legacy-maintenance-boundary.json.sha256").write_text(
    f"{hashlib.sha256(boundary_raw).hexdigest()}  legacy-maintenance-boundary.json\n",encoding="ascii")
for path in root.rglob("*"):
    if path.is_file(): path.chmod(0o400)
(root / "fixture.txt").write_text(f"{freeze_sha}\n{capture}\n", encoding="ascii")
PY
    read -r freeze_sha < "$f/fixture.txt"
    capture="$(sed -n '2p' "$f/fixture.txt")"
    receipt_sha="$(create_offline_stop_evidence "$f/freeze.json" "$freeze_sha" \
        "$capture" "$f/status" "$f/offline-stop.json" \
        2026-08-28T12:00:00Z 2026-08-28T12:00:01Z \
        "$f/legacy-height-cross-proof.json" "$f/legacy-maintenance-boundary.json" \
        "$f/legacy-maintenance-evidence-bundle.json")" || return 1
    python3 - "$f/offline-stop.json" "$receipt_sha" <<'PY' || return 1
import hashlib, json, pathlib, stat, sys
path = pathlib.Path(sys.argv[1]); expected = sys.argv[2]
raw = path.read_bytes(); value = json.loads(raw)
assert raw == (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
assert value["schema"] == "arc.validator-vault.offline-stop-evidence.v2"
assert value["first_quarantine_started_at"] == "2026-08-28T12:00:00Z"
assert value["all_controlled_stopped_at"] == "2026-08-28T12:00:01Z"
assert value["legacy_height_cross_proof"]["conservative_height_floor"] == 105
assert [row["node"] for row in value["nodes"]] == ["nyc", "lax", "ams", "lhr", "nrt", "sgp"]
assert hashlib.sha256(raw).hexdigest() == expected
assert stat.S_IMODE(path.stat().st_mode) == 0o400
assert stat.S_IMODE(path.with_name(path.name + ".sha256").stat().st_mode) == 0o400
PY
    python3 - "$f" <<'PY' || return 1
import base64, json, pathlib, struct, sys
root = pathlib.Path(sys.argv[1])
fleet = (
    ("nyc", "149.28.32.76"), ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"), ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"), ("sgp", "149.28.153.31"),
)
known = []
challenged = root / "challenged"; challenged.mkdir()
challenge = "c" * 64
for index, (node, host) in enumerate(fleet):
    blob = struct.pack(">I", 11) + b"ssh-ed25519" + struct.pack(">I", 32) + bytes([index + 1]) * 32
    known.append(f"{host} ssh-ed25519 {base64.b64encode(blob).decode()}\n")
    status = json.loads((root / "status" / f"{node}-stopped-status.json").read_text())
    status.update(schema="arc.recovery.offline-stop-challenged-status.v1", host=host, challenge=challenge)
    (challenged / f"{node}-challenged-status.json").write_text(
        json.dumps(status, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8"
    )
(root / "known_hosts").write_text("".join(known), encoding="ascii")
(root / "id_ed25519").write_text("test-only-private-key\n", encoding="ascii")
for path in (root / "known_hosts", root / "id_ed25519"):
    path.chmod(0o400)
PY
    local known_sha verification python_path python_sha ssh_sha phase_verification
    local expected_fixture_freeze_sha
    known_sha="$(hash_file "$f/known_hosts")"
    verify_offline_stop_inputs "$f/freeze.json" "$freeze_sha" "$f/offline-stop.json" \
        "$receipt_sha" "$f/known_hosts" "$known_sha" "$f/id_ed25519" || return 1
    verification="$(build_offline_stop_remote_verification \
        "$f/freeze.json" "$freeze_sha" "$f/offline-stop.json" "$receipt_sha" \
        "$known_sha" "$(printf 'c%.0s' {1..64})" 2026-08-28T12:00:00Z \
        2026-08-28T12:00:01Z 1001 "$f/challenged" "$(printf 'd%.0s' {1..64})")" || return 1
    python3 -c 'import json,sys; value=json.loads(sys.argv[1]); assert value["schema"]=="arc.recovery.offline-stop-remote-verification.v1" and len(value["nodes"])==6 and value["nodes"][0]["status"]["challenge"]=="c"*64 and value["nodes"][5]["host"]=="149.28.153.31"' \
        "$verification" || return 1
    python_path="$(python3 -c 'import os; print(os.path.realpath("/usr/bin/python3"))')" || return 1
    python_sha="$(python3 - "$python_path" <<'PY'
import hashlib, pathlib, sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" || return 1
    ssh_sha="$(python3 - <<'PY'
import hashlib, pathlib
print(hashlib.sha256(pathlib.Path("/usr/bin/ssh").read_bytes()).hexdigest())
PY
)" || return 1
    # Exercise the real protected macOS/Linux tool path.  On macOS this proves
    # the signed-system Python's legitimate nlink>1 shape works even though
    # `/usr/bin/stat -c` is rejected by BSD stat.  The static assertion keeps
    # the Linux regression from accidentally reintroducing either GNU-only call.
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
phase = text[text.index("verify_offline_stop_phase()") : text.index("create_offline_stop_evidence()")]
assert "/usr/bin/stat -c" not in phase
assert "/usr/bin/readlink -f" not in phase
assert "verify_offline_stop_transport_tools" in phase
PY
    ( verify_offline_stop_transport_tools "$python_path" "$(printf '0%.0s' {1..64})" "$ssh_sha" ) \
        >/dev/null 2>&1 && return 1
    ( verify_offline_stop_transport_tools /tmp/python3 "$python_sha" "$ssh_sha" ) \
        >/dev/null 2>&1 && return 1
    expected_fixture_freeze_sha="$freeze_sha"
    # shellcheck disable=SC2329 # invoked indirectly by the sourced orchestrator
    freeze_plan_hash() { printf '%s\n' "$expected_fixture_freeze_sha"; }
    # shellcheck disable=SC2329 # invoked indirectly by the sourced orchestrator
    run_stopped_status_challenged_exact() {
        local node="$4"
        /bin/cat "$f/challenged/$node-challenged-status.json"
    }
    phase_verification="$(verify_offline_stop_phase \
        --freeze-plan "$f/freeze.json" \
        --offline-stop-evidence "$f/offline-stop.json" \
        --offline-stop-evidence-sha256 "$receipt_sha" \
        --ssh-known-hosts "$f/known_hosts" \
        --ssh-known-hosts-sha256 "$known_sha" \
        --ssh-identity "$f/id_ed25519" \
        --python-path "$python_path" --python-sha256 "$python_sha" \
        --ssh-sha256 "$ssh_sha" --challenge "$(printf 'c%.0s' {1..64})")" || return 1
    python3 -c 'import json,sys; value=json.loads(sys.argv[1]); assert value["schema"]=="arc.recovery.offline-stop-remote-verification.v1" and len(value["nodes"])==6 and value["ssh_sha256"]==sys.argv[2]' \
        "$phase_verification" "$ssh_sha" || return 1
    local resumed_sha
    resumed_sha="$(create_offline_stop_evidence "$f/freeze.json" "$freeze_sha" "$capture" \
        "$f/status" "$f/offline-stop.json" 2026-08-28T12:00:00Z \
        2026-08-28T12:00:01Z "$f/legacy-height-cross-proof.json" \
        "$f/legacy-maintenance-boundary.json" \
        "$f/legacy-maintenance-evidence-bundle.json")" || return 1
    [ "$resumed_sha" = "$receipt_sha" ] || return 1
    # A crash after a complete partial fsync but before rename, and a crash
    # after the primary rename but before the ordered sidecar, are both
    # resumable without resampling any irreversible maintenance input.
    mv "$f/offline-stop.json" "$f/offline-stop.json.partial"
    rm -f -- "$f/offline-stop.json.sha256"
    resumed_sha="$(create_offline_stop_evidence "$f/freeze.json" "$freeze_sha" "$capture" \
        "$f/status" "$f/offline-stop.json" 2026-08-28T12:00:00Z \
        2026-08-28T12:00:01Z "$f/legacy-height-cross-proof.json" \
        "$f/legacy-maintenance-boundary.json" \
        "$f/legacy-maintenance-evidence-bundle.json")" || return 1
    [ "$resumed_sha" = "$receipt_sha" ] || return 1
    printf '{"schema":' > "$f/truncated.json.partial"
    chmod 400 "$f/truncated.json.partial"
    create_offline_stop_evidence "$f/freeze.json" "$freeze_sha" "$capture" \
        "$f/status" "$f/truncated.json" 2026-08-28T12:00:00Z \
        2026-08-28T12:00:01Z "$f/legacy-height-cross-proof.json" \
        "$f/legacy-maintenance-boundary.json" \
        "$f/legacy-maintenance-evidence-bundle.json" >/dev/null || return 1
    [ -s "$f/truncated.json" ] && [ ! -e "$f/truncated.json.partial" ] || return 1
    chmod 600 "$f/status/nyc-stopped-status.json"
    python3 - "$f/status/nyc-stopped-status.json" "$f/status/lax-stopped-status.json" <<'PY'
import json, pathlib, sys
left, right = map(pathlib.Path, sys.argv[1:])
value = json.loads(left.read_text()); value["stop_complete_sha256"] = json.loads(right.read_text())["stop_complete_sha256"]
left.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")
PY
    chmod 400 "$f/status/nyc-stopped-status.json"
    ( create_offline_stop_evidence "$f/freeze.json" "$freeze_sha" "$capture" \
        "$f/status" "$f/duplicate.json" 2026-08-28T12:00:00Z \
        2026-08-28T12:00:01Z "$f/legacy-height-cross-proof.json" \
        "$f/legacy-maintenance-boundary.json" \
        "$f/legacy-maintenance-evidence-bundle.json" ) >/dev/null 2>&1 && return 1
    return 0
)

ordinary_and_challenged_stopped_status_execute() (
    # shellcheck source=/dev/null
    . "$NODE_HELPER" >/dev/null
    local f capture freeze challenge validator root base challenged
    f="$(mktemp -d)"; trap 'chmod -R u+w "$f" 2>/dev/null || true; rm -rf -- "$f"' EXIT
    capture="$(printf 'a%.0s' {1..64})"; freeze="$(printf 'b%.0s' {1..64})"
    challenge="$(printf 'c%.0s' {1..64})"; validator="$(printf 'd%.0s' {1..64})"
    STOP_BASE="$f/stops"; root="$STOP_BASE/$capture/nyc"; mkdir -p "$root/evidence"
    python3 - "$root/evidence/writer-contract.json" "$freeze" "$validator" <<'PY' || return 1
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
value = {
    "schema": "arc.recovery.exact-writer.v3",
    "freeze_plan_sha256": sys.argv[2],
    "validator_address": sys.argv[3],
    "stake": 5_000_000,
}
path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
PY
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    verify_tree_index() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    verify_stop_identity() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    verify_sealed_stop_contract() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    verify_stop_journal_semantics() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    verify_legacy_restart_fence() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    pgrep() { return 1; }
    # shellcheck disable=SC2329 # invoked indirectly by stopped_status
    hash_file() {
        case "$1" in
            */stop.complete) printf '%064d\n' 1 ;;
            */stop.files.sha256) printf '%064d\n' 2 ;;
            *) return 1 ;;
        esac
    }
    base="$(stopped_status "$capture" nyc)" || return 1
    challenged="$(stopped_status_challenged \
        "$capture" nyc "$freeze" "$validator" 5000000 101 202 \
        00000000-0000-0000-0000-000000000001 "$(printf '3%.0s' {1..64})" \
        systemd-unit arc-node.service 101 202 /usr/bin/systemd \
        "$(printf '4%.0s' {1..64})" "$(printf '5%.0s' {1..64})" \
        "$(printf '6%.0s' {1..64})" /usr/local/bin/arc-node \
        "$(printf '7%.0s' {1..64})" "$(printf '8%.0s' {1..64})" \
        /var/lib/arc-data 149.28.32.76 "$challenge")" || return 1
    python3 -c 'import json,sys; base=json.loads(sys.argv[1]); challenged=json.loads(sys.argv[2]); assert base["schema"]=="arc.recovery.offline-stop-status.v1" and base["stopped"] is True and base["restart_fenced"] is True and base["capture_id"]==sys.argv[3] and base["freeze_plan_sha256"]==sys.argv[4] and base["validator_address"]==sys.argv[5]; assert challenged==dict(base,schema="arc.recovery.offline-stop-challenged-status.v1",host="149.28.32.76",challenge=sys.argv[6])' \
        "$base" "$challenged" "$capture" "$freeze" "$validator" "$challenge" || return 1
    ( stopped_status_challenged \
        "$capture" nyc "$freeze" "$validator" 5000000 101 202 \
        00000000-0000-0000-0000-000000000001 "$(printf '3%.0s' {1..64})" \
        systemd-unit arc-node.service 101 202 /usr/bin/systemd \
        "$(printf '4%.0s' {1..64})" "$(printf '5%.0s' {1..64})" \
        "$(printf '6%.0s' {1..64})" /usr/local/bin/arc-node \
        "$(printf '7%.0s' {1..64})" "$(printf '8%.0s' {1..64})" \
        /var/lib/arc-data 192.0.2.99 "$challenge" ) >/dev/null 2>&1 && return 1
    return 0
)

content_capture_fixture_detects_source_tamper() (
    # shellcheck source=/dev/null
    . "$NODE_HELPER" >/dev/null
    local f c d id n freeze live generation generation_sha drive_sha; f="$(mktemp -d)"; trap 'chmod -R u+w "$f" 2>/dev/null || true; rm -rf -- "$f"' EXIT
    c="$f/capture"; d="$f/legacy-data"; id="$(printf 'a%.0s' {1..64})"; freeze="$(printf 'b%.0s' {1..64})"; n=nyc
    generation="$(printf 'c%.0s' {1..64})"; generation_sha="$(printf 'd%.0s' {1..64})"; drive_sha="$(printf 'e%.0s' {1..64})"
    LIVE_OBSERVATION_BASE="$f/live"; live="$LIVE_OBSERVATION_BASE/$id/$generation/$n"
    mkdir -p "$c" "$d" "${live%/*}"; printf sealed-wal > "$d/state.wal"; printf state > "$d/state.bin"
    capture_live_observation_receipt_at "${live%/*}/.${n}.live-observations.partial" \
        "$live" "$id" "$generation" \
        "$generation_sha" "$drive_sha" "$n" "$freeze" http://127.0.0.1:1 || return 1
    write_regular_tree_inventory "$d" "$c/source-data.files.sha256" || return 1
    python3 - "$d" "$c/capture-source.json" <<'PY' || return 1
import hashlib,json,pathlib,sys
r,o=map(pathlib.Path,sys.argv[1:]); w=r/"state.wal"; files=[p for p in r.rglob("*") if p.is_file()]
v={"schema":"arc.recovery.capture-source.v1","data_dir":str(r),"data_device":r.stat().st_dev,"data_inode":r.stat().st_ino,"data_bytes":sum(p.stat().st_size for p in files),"data_files":len(files),"state_wal_bytes":w.stat().st_size,"state_wal_sha256":hashlib.sha256(w.read_bytes()).hexdigest(),"external_snapshots":[]}
o.write_text(json.dumps(v,sort_keys=True,separators=(",",":"))+"\n")
PY
    printf 'capture_id=%s\nnode=%s\nfreeze_plan_sha256=%s\nlegacy_live_observations_schema=arc.recovery.legacy-live-observations.v1\nlegacy_live_observations_generation=%s\nlegacy_live_observations_generation_receipt_sha256=%s\nlegacy_live_observations_drive_prefreeze_receipt_sha256=%s\nlegacy_live_observations_root_sha256=%s\nlegacy_live_observations_receipt_sha256=%s\nlegacy_live_observations_labels=diagnostic,noncanonical,nonreward\n' \
        "$id" "$n" "$freeze" "$generation" "$generation_sha" "$drive_sha" \
        "$(hash_file "$live/live-observations.files.sha256")" \
        "$(hash_file "$live/receipt.json")" > "$c/capture.inventory"
    write_tree_index "$c" capture.files.sha256 capture.complete || return 1
    write_complete_marker "$c" capture.files.sha256 capture.complete arc.recovery.capture.v4 "capture_id=$id" "node=$n" || return 1
    verify_capture_source "$c" "$id" "$n" || return 1
    printf tamper >> "$d/state.bin"; ( verify_capture_source "$c" "$id" "$n" ) >/dev/null 2>&1 && return 1
    [ ! -d "$c/data-dir" ]
)

partial_retry_ownership_rejects_symlink_and_foreign_marker() (
    # shellcheck source=/dev/null
    . "$NODE_HELPER" >/dev/null
    local f p; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT; p="$f/partial"
    ln -s "$f" "$p"; ( prepare_owned_partial_directory "$p" exact ) >/dev/null 2>&1 && return 1
    rm "$p"; mkdir "$p"; printf 'foreign\n' > "$p/.arc-recovery-partial-owner"
    ( prepare_owned_partial_directory "$p" exact ) >/dev/null 2>&1 && return 1
    printf 'exact\n' > "$p/.arc-recovery-partial-owner"; printf x > "$p/stale"
    prepare_owned_partial_directory "$p" exact || return 1
    [ ! -e "$p/stale" ] && [ "$(cat "$p/.arc-recovery-partial-owner")" = exact ]
)

live_observations_are_bounded_create_only_and_resumable() (
    # shellcheck source=/dev/null
    . "$NODE_HELPER" >/dev/null
    local f port server id freeze generation generation_sha drive_sha first first_count first_sha
    f="$(mktemp -d)"; server=""
    trap '[ -z "$server" ] || { kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; }; chmod -R u+w "$f" 2>/dev/null || true; find "$f" -depth -delete' EXIT
    cat > "$f/server.py" <<'PY'
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import pathlib, sys
log, port_file = map(pathlib.Path, sys.argv[1:])
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        with log.open("a", encoding="utf-8") as handle:
            handle.write(self.path + "\n")
        if self.path == "/inference/results":
            status, body = 200, b'{"results":[1]}'
        elif self.path == "/workers/scoreboard":
            status, body = 200, b'x' * (8 * 1024 * 1024 + 1)
        elif self.path == "/inference/attestations":
            status, body = 404, b'{"error":"legacy endpoint absent"}'
        else:
            status, body = 500, b'unexpected endpoint'
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try: self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError): pass
    def log_message(self, *_): pass
server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
port_file.write_text(str(server.server_port), encoding="ascii")
server.serve_forever()
PY
    : > "$f/requests.log"
    "$ARC_RECOVERY_PYTHON_PATH" -I "$f/server.py" "$f/requests.log" "$f/port" & server=$!
    for _ in $(seq 1 100); do [ -s "$f/port" ] && break; sleep 0.02; done
    [ -s "$f/port" ] || return 1
    port="$(cat "$f/port")"; id="$(printf 'a%.0s' {1..64})"; freeze="$(printf 'b%.0s' {1..64})"
    generation="$(printf 'c%.0s' {1..64})"; generation_sha="$(printf 'd%.0s' {1..64})"; drive_sha="$(printf 'e%.0s' {1..64})"
    LIVE_OBSERVATION_BASE="$f/live"; first="$LIVE_OBSERVATION_BASE/$id/$generation/nyc"
    mkdir -p -- "${first%/*}"

    capture_live_observation_receipt_at "${first%/*}/.nyc.live-observations.partial" \
        "$first" "$id" "$generation" \
        "$generation_sha" "$drive_sha" nyc "$freeze" \
        "http://127.0.0.1:$port" || return 1
    verify_live_observation_receipt "$first" "$id" "$generation" "$generation_sha" \
        "$drive_sha" nyc "$freeze" || return 1
    first_sha="$(hash_file "$first/receipt.json")"
    verify_live_observation_receipt "$first" "$id" "$generation" "$generation_sha" \
        "$drive_sha" nyc "$freeze" || return 1
    [ "$(hash_file "$first/receipt.json")" = "$first_sha" ] || return 1
    live_observations_status "$id" "$generation" "$generation_sha" "$drive_sha" nyc "$freeze" >/dev/null || return 1
    ( live_observations_status "$id" "$(printf 'f%.0s' {1..64})" "$generation_sha" \
        "$drive_sha" nyc "$freeze" ) >/dev/null 2>&1 && return 1
    python3 - "$first/receipt.json" "$f/requests.log" <<'PY' || return 1
import json, pathlib, sys
receipt = json.loads(pathlib.Path(sys.argv[1]).read_text())
paths = pathlib.Path(sys.argv[2]).read_text().splitlines()
assert paths == ["/inference/results", "/workers/scoreboard", "/inference/attestations"]
assert "/community/list" not in paths
assert receipt["labels"] == ["diagnostic", "noncanonical", "nonreward"]
assert receipt["diagnostic"] is True and receipt["canonical"] is False and receipt["reward_evidence"] is False
rows = receipt["observations"]
assert rows[0]["http_status"] == 200 and rows[0]["raw_complete"] is True
assert rows[1]["http_status"] == 200 and rows[1]["error"] == "response_body_limit_exceeded"
assert rows[1]["raw_bytes"] == 8 * 1024 * 1024 and rows[1]["raw_complete"] is False
assert rows[2]["http_status"] == 404 and rows[2]["raw_complete"] is True
PY

    : > "$f/requests.log"
    python3 - "$f/.resume.partial" "$id" "$generation" "$generation_sha" "$drive_sha" "$freeze" <<'PY' || return 1
import json, pathlib, sys
root = pathlib.Path(sys.argv[1]); root.mkdir(); (root/"journal").mkdir(); (root/"observations").mkdir(); (root/"raw").mkdir()
(root/".arc-recovery-partial-owner").write_text(
    f"schema=arc.recovery.live-observation-partial.v1 capture={sys.argv[2]} generation={sys.argv[3]} generation_receipt={sys.argv[4]} drive_receipt={sys.argv[5]} node=lax freeze={sys.argv[6]}\n"
)
attempt = {"schema":"arc.recovery.legacy-live-observation-attempt.v1","endpoint":"/inference/results","started_at":"2026-08-28T00:00:00.000000Z","node":"lax","observation_generation":sys.argv[3]}
(root/"journal/00-inference-results.attempt.json.partial").write_text(json.dumps(attempt,sort_keys=True,separators=(",",":"))+"\n")
(root/"observations/00-inference-results.json.partial").write_text('{"truncated"')
(root/"raw/00-inference-results.body.partial").write_bytes(b"durable-prefix")
(root/"receipt.json.partial").write_text('{"truncated"')
for path in (root/".arc-recovery-partial-owner", root/"journal/00-inference-results.attempt.json.partial"):
    path.chmod(0o400)
for path in (root/"observations/00-inference-results.json.partial",
             root/"raw/00-inference-results.body.partial",root/"receipt.json.partial"):
    path.chmod(0o600)
PY
    capture_live_observation_receipt_at "$f/.resume.partial" "$f/resumed" "$id" "$generation" \
        "$generation_sha" "$drive_sha" lax "$freeze" \
        "http://127.0.0.1:$port" || return 1
    verify_live_observation_receipt "$f/resumed" "$id" "$generation" "$generation_sha" \
        "$drive_sha" lax "$freeze" || return 1
    python3 - "$f/resumed/receipt.json" "$f/requests.log" <<'PY' || return 1
import json, pathlib, sys
receipt=json.loads(pathlib.Path(sys.argv[1]).read_text()); paths=pathlib.Path(sys.argv[2]).read_text().splitlines()
assert receipt["observations"][0]["error"] == "interrupted_after_durable_attempt_intent"
assert receipt["observations"][0]["raw_bytes"] == len(b"durable-prefix")
assert "/inference/results" not in paths
assert paths == ["/workers/scoreboard", "/inference/attestations"]
PY

    ln -s "$f" "$f/.symlink.partial"
    ( capture_live_observation_receipt_at "$f/.symlink.partial" "$f/never" "$id" "$generation" \
        "$generation_sha" "$drive_sha" ams "$freeze" \
        "http://127.0.0.1:$port" ) >/dev/null 2>&1 && return 1
    chmod u+w "$first/receipt.json"; printf mutation >> "$first/receipt.json"
    ( verify_live_observation_receipt "$first" "$id" "$generation" "$generation_sha" \
        "$drive_sha" nyc "$freeze" ) >/dev/null 2>&1 && return 1
    python3 - "$NODE_HELPER" "$ORCHESTRATOR" <<'PY' || return 1
import pathlib, sys
node, fleet = (pathlib.Path(path).read_text() for path in sys.argv[1:])
capture = node[node.index("capture_live_observations()") : node.index("verify_merged_legacy_fence_config()")]
reuse = capture.index('if [ -e "$root" ] || [ -L "$root" ]')
stop_guard = capture.index('refusing first live-observation receipt after this writer was stopped/fenced')
network = capture.index('capture_live_observation_receipt_at')
assert reuse < stop_guard < network
phase = fleet[fleet.index("capture_phase()") : fleet.index("manifest_field()")]
assert phase.index('if [ "$execute" != true ]') < phase.index("capture_all_live_observations")
fleet_capture = fleet[fleet.index("capture_all_live_observations()") : fleet.index("run_sealed_source_status_exact()")]
assert fleet_capture.index("run_live_observations_eligibility_exact") < fleet_capture.index("run_live_observations_exact")
assert "complete_count" in fleet_capture and "recapture is forbidden" in fleet_capture
assert '"${live_root#/}"' in node[node.index("stream_bundle()") : node.index("stream_inventory()")]
assert 'legacy-live-observations.json' in fleet
PY
    first_count="$(wc -l < "$f/requests.log" | tr -d ' ')"
    [ "$first_count" = 2 ]
)

disk_peak_allows_growth_and_reserves_v3() {
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
t=pathlib.Path(sys.argv[1]).read_text(); gib=1024**3
def required(current): return current+max(current,2*gib)+2*gib
assert required(29*gib)==60*gib and 63*gib>=required(29*gib) and 59*gib<required(29*gib)
assert 63*gib<29*gib*3+2*gib
assert required(30*gib)==62*gib and 63*gib>=required(30*gib) # normal post-plan growth passes
assert "new_v3_headroom_bytes = data_bytes" in t and "archive_stream_temporary_bytes = 0" in t
assert "bytes * 3" not in t and "files * 3" not in t
assert 'test "$bytes" -le "$sealed_data_bytes"' not in t and "sealed_data_bytes" not in t
assert t.count('required_bytes=$((bytes + binding_bytes))') >= 2
PY
}

wal_boundary_hashes_without_duplicate_files() (
    # shellcheck source=/dev/null
    . "$NODE_HELPER" >/dev/null
    local f; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT; mkdir "$f/evidence"
    printf 'committed-tail' > "$f/state.wal"
    printf '%s\n' '{"source_wal_original_bytes":14,"source_wal_accepted_prefix_bytes":9,"source_wal_quarantined_tail_bytes":5,"source_wal_tail_reason":"uncommitted tail"}' > "$f/summary.json"
    write_offline_wal_evidence "$f/state.wal" "$f/summary.json" "$f/evidence.json" "$f/evidence" || return 1
    python3 - "$f/evidence.json" <<'PY' || return 1
import hashlib,json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text()); assert v["accepted_prefix_sha256"]==hashlib.sha256(b"committed").hexdigest(); assert v["quarantined_tail_sha256"]==hashlib.sha256(b"-tail").hexdigest(); assert v["prefix_plus_tail_reconstructs_capture"]
PY
    [ -z "$(find "$f/evidence" -type f -print -quit)" ]
)

model_size_hash_and_shards_are_bound() {
    PYTHONPATH="${ROLLOUT%/*}" python3 - "$ORCHESTRATOR" "$SCHEMA" <<'PY' || return 1
import json,pathlib,sys,recovery_rollout as rr
assert rr.CANONICAL_MODEL_SIZE_BYTES==4_081_004_224
assert rr.CANONICAL_MODEL_SHA256=="08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
assert json.loads(pathlib.Path(sys.argv[2]).read_text())["$defs"]["validator"]["properties"]["model_size_bytes"]["const"]==rr.CANONICAL_MODEL_SIZE_BYTES
t=pathlib.Path(sys.argv[1]).read_text()
for n,r in {"nyc":[[0,6],[22,27],[27,32]],"lax":[[0,6],[6,12],[27,32]],"ams":[[0,6],[6,12],[12,17]],"lhr":[[6,12],[12,17],[17,22]],"nrt":[[12,17],[17,22],[22,27]],"sgp":[[17,22],[22,27],[27,32]]}.items(): assert f'"{n}": {r}' in t
PY
    grep -Fq 'prove_production_runtime_inventory' "$ROLLOUT" && grep -Fq 'sha256sum "/proc/$pid/cmdline"' "$ROLLOUT"
}

stream_has_no_full_copy_or_model_member() {
    grep -Fq 'stream-bundle' "$NODE_HELPER" && grep -Fq 'rclone rcat "$partial_remote"' "$ORCHESTRATOR" || return 1
    grep -Fq 'model_excluded_and_bound_by_rollout=true' "$NODE_HELPER" || return 1
    ! grep -Fq 'offline-working-data' "$NODE_HELPER" && ! grep -Fq 'copy_stopped_data_tree' "$NODE_HELPER" || return 1
    ! grep -Fq 'run_remote "$node" bundle ' "$ORCHESTRATOR" || return 1
    ! sed -n '/^ACTION=/,$p' "$NODE_HELPER" | grep -Eq '^[[:space:]]*(bundle|bundle-status)\)' || return 1
    grep -Fq 'local partial_root="${destination%/*}/.arc-recovery-partials/$capture_id/$manifest_sha"' "$ORCHESTRATOR" || return 1
    grep -Fq 'st_dev != root_device' "$NODE_HELPER" "$ORCHESTRATOR" || return 1
    ! grep -Fq 'source_tree_immutable_in_place' "$NODE_HELPER"
}

v5_freeze_transaction_is_fault_closed() {
    local old_stop old_continue old_quiesce
    old_stop='SIG''STOP'; old_continue='SIG''CONT'; old_quiesce='pidfd-quiesce-''intent'
    for required in \
      'arc.recovery.freeze-plan.v5' \
      'writer_cgroup_path' 'writer_cgroup_device' 'writer_cgroup_inode' \
      'prepare_barrier' 'arc.recovery.prepare-barrier.v1' \
      'ConditionPathExists=/etc/arc-recovery/legacy-start-allowed' \
      '/run/systemd/system.control' \
      'arc.recovery.restart-barrier-arm.v1' \
      'arc.recovery.restart-barrier-committed.v2' \
      'arc.recovery.fast-cgroups-frozen.v1' \
      'arc.recovery.cgroup-thaw-intent.v2' \
      'arc.recovery.detached-writer-terminal.v2' \
      'DefaultDependencies=no'; do
        grep -Fq -- "$required" "$ORCHESTRATOR" "$NODE_HELPER" || return 1
    done
    grep -Fq 'writer_supervision_mode = "detached-root-session"' "$ORCHESTRATOR" || return 1
    grep -Fq 'signal.pidfd_send_signal(descriptor, signal.SIGTERM' "$NODE_HELPER" || return 1
    grep -Fq 'SIGKILL is forbidden' "$NODE_HELPER" || return 1
    ! grep -Eq "signal\\.$old_stop|signal\\.$old_continue|$old_quiesce|$old_stop-sent|$old_continue-sent" "$NODE_HELPER" || return 1
    ! grep -Eq 'normalize_writer|reparent_writer|normalized into its reviewed systemd supervisor' "$NODE_HELPER" "$ORCHESTRATOR" || return 1
    ! grep -Eq 'pkill[[:space:]]|killall[[:space:]]|kill[[:space:]]+-9|disable[[:space:]]+--now[[:space:]].*(arc-node|arc-self-heal)' "$NODE_HELPER" "$ORCHESTRATOR" || return 1
    python3 - "$NODE_HELPER" "$ORCHESTRATOR" <<'PY' || return 1
import pathlib
import sys

node, orchestrator = (pathlib.Path(path).read_text() for path in sys.argv[1:])
fast = node[node.index("fast_cgroup_freeze()"):node.index("pre_fence_quiesce_phase()")]
assert fast.index('freeze_targets = [cgroup_entry("supervisor", supervisor_cgroup)]') < fast.index(
    'freeze_targets.append(prepared_parent)'
)
parent_frozen = fast.index('02-writer-parent-cgroup-frozen.json')
move_intent = fast.index('publish(leaf_move_path, move_intent)')
leaf_create = fast.index('os.mkdir(leaf_name, 0o755, dir_fd=parent_directory)')
leaf_local_freeze = fast.index('os.write(freezer, b"1")', leaf_create)
writer_move = fast.index('os.write(procs, str(writer_pid).encode("ascii"))', leaf_local_freeze)
leaf_receipt = fast.index('publish(leaf_receipt_path', writer_move)
parent_release = fast.index('write_local_freeze(prepared_parent, 0)', leaf_receipt)
release_receipt = fast.index('publish(parent_release_path', parent_release)
assert parent_frozen < move_intent < leaf_create < leaf_local_freeze < writer_move
assert writer_move < leaf_receipt < parent_release < release_receipt
assert 'arc.recovery.detached-writer-leaf-move-intent.v1' in fast
assert 'arc.recovery.detached-writer-parent-release.v1' in fast
assert 'leaf.joinpath("cgroup.freeze").read_text' in node
release_retry = fast.index('parent_already_released = False')
release_skip = fast.index('entry["role"] == "writer-parent" and parent_already_released', release_retry)
parent_refreeze = fast.index('write_local_freeze(entry, 1)', release_skip)
assert release_retry < release_skip < parent_refreeze
assert 'A durable parent-release receipt is a one-way phase transition' in fast
assert 'cgroup_subtree_pids(prepared_parent["path"]) != [writer_pid]' in fast
assert 'DefaultDependencies=no' in fast
assert 'exact_scope["DefaultDependencies"] != "no"' in fast
assert 'exact_scope["Conflicts"] or exact_scope["Before"]' in fast
assert fast.index('entry["role"] == "supervisor"') < fast.index(
    'entry["role"] == "writer-parent" and writer_mode == "detached-root-session"'
)

barrier = node[node.index("# Current v5 barrier transaction."):]
mask_create = barrier.index('os.symlink("/dev/null", unit, dir_fd=control)')
arm_publish = barrier.index('"schema": "arc.recovery.restart-barrier-arm.v1"')
marker_unlink = barrier.index('os.unlink("legacy-start-allowed", dir_fd=parent)')
assert mask_create < arm_publish < marker_unlink
mask_loop = barrier[barrier.rindex("control = os.open", 0, mask_create):barrier.index(
    "systemctl daemon-reload", mask_create
)]
assert "if value != selected" not in mask_loop
arm_loop = barrier[barrier.index("barriers = {}; runtime = {}; masks = {}"):arm_publish]
assert 'if unit == selected' not in arm_loop
assert 'masks[unit] = "/dev/null"' in arm_loop
assert 'run_mounts != ["tmpfs"]' in barrier
assert '/etc/systemd/system.control/' in barrier

controller = node[node.index("def thaw(entry, intent_path):"):node.index("def term_progress(prefix):")]
assert '["systemctl", "thaw"' not in controller
opened = controller.index("directory = os.open(")
inode_check = controller.index('details.st_dev != entry["device"]', opened)
direct_thaw = controller.index('os.write(freezer, b"0")', inode_check)
assert opened < inode_check < direct_thaw
detached = node[node.index('# Detached mode is deliberately two-stage.'):node.index(
    'event("50-cgroups-thawed.json"'
)]
writer_term = detached.index('ensure_term(writer_target)')
writer_intent = detached.index('"detached-writer-first"', writer_term)
writer_thaw = detached.index('thaw(role_entries["writer"], thaw_intent_path)', writer_intent)
terminal = detached.index('event(writer_terminal_path.name', writer_thaw)
supervisor_term = detached.index('ensure_term(supervisor_targets[0])', terminal)
supervisor_intent = detached.index('"detached-supervisor-after-writer-terminal"', supervisor_term)
supervisor_thaw = detached.index('thaw(role_entries["supervisor"], supervisor_intent_path)', supervisor_intent)
assert writer_term < writer_intent < writer_thaw < terminal
assert terminal < supervisor_term < supervisor_intent < supervisor_thaw
assert 'term_progress("20-supervisor") != "missing"' in detached
assert '"supervisor_containment"' in detached
assert "stable_absence_checks" in detached
assert 'An unsignaled supervisor disappearance has no durable causal/ordering' in detached
assert 'sealed-supervisor-terminal-cgroup-disappeared' not in node
assert '"parent_state": scope_parent_state' in barrier
assert 'terminal-after-leaf-seal' in barrier

assert 'expected_go="STAGE-BARRIERS $orchestrator_sha HELPER $helper_sha"' in orchestrator
assert 'expected_go="FREEZE $freeze_sha CAPTURE $capture_id"' in orchestrator
assert 'expected_go="GO $manifest_sha FREEZE $freeze_sha CAPTURE $capture_id DEST $destination_sha LEGACY_WAL $policy"' in orchestrator
capture = orchestrator[
    orchestrator.index("capture_phase()") : orchestrator.index("manifest_field()")
]
assert capture.index("run_drive_prefreeze_gate execute") < capture.index(
    'capture_all_live_observations "$freeze_plan" "$freeze_sha" "$capture_id"'
)
assert capture.index('capture_all_live_observations "$freeze_plan" "$freeze_sha" "$capture_id"') < capture.index(
    'run_quarantine_generation_rounds'
)
assert capture.index('run_quarantine_generation_rounds') < capture.index(
    'stop_after_quarantine_round_exact'
)
PY
}
v5_stop_journal_semantics_are_fault_closed() {
    local old_stop_schema
    old_stop_schema='arc.recovery.offline-stop.v''3'
    for required in \
      'arc.recovery.offline-stop.v4' \
      'same-boot-frozen-cgroup-controller' \
      'cgroup-v2-freeze' \
      'SIGTERM-sent-via-pidfd-while-cgroup-frozen' \
      'arc.recovery.cgroup-thaw-intent.v2' \
      'no_signal_replayed_after_own_stage_thaw_intent' \
      'sealed-boot-ended; no stale PID signaled' \
      'exit_cause": "unknown"'; do
        grep -Fq -- "$required" "$NODE_HELPER" || return 1
    done
    ! grep -Fq "$old_stop_schema" "$NODE_HELPER" || return 1
    PYTHONDONTWRITEBYTECODE=1 python3 "$FREEZE_MODULE_TEST" >/dev/null || return 1
    python3 - "$NODE_HELPER" "$FREEZE_MODULE" <<'PY' || return 1
import importlib.util
import pathlib
import sys

node_path, module_path = map(pathlib.Path, sys.argv[1:])
node = node_path.read_text()
stop = node[node.index("stop_node_cleanly()"):node.index("reconcile_known_stop_partials()")]
reboot_branch = stop.index("if current_boot_id != boot_id:")
detached_start = stop.index("# Detached mode is deliberately two-stage.", reboot_branch)
writer_term = stop.index("ensure_term(writer_target)", detached_start)
writer_intent = stop.index('"detached-writer-first"', writer_term)
writer_thaw = stop.index('thaw(role_entries["writer"], thaw_intent_path)', writer_intent)
writer_terminal = stop.index('event(writer_terminal_path.name', writer_thaw)
supervisor_term = stop.index('ensure_term(supervisor_targets[0])', writer_terminal)
supervisor_intent = stop.index('"detached-supervisor-after-writer-terminal"', supervisor_term)
supervisor_thaw = stop.index('thaw(role_entries["supervisor"], supervisor_intent_path)', supervisor_intent)
assert reboot_branch < writer_term < writer_intent < writer_thaw < writer_terminal
assert writer_terminal < supervisor_term < supervisor_intent < supervisor_thaw
assert 'signal.pidfd_send_signal(descriptor, signal.SIGTERM, None, 0)' in stop
assert "signal.SIGKILL" not in stop
assert '"recovery_sigkill_allowed": False' in stop
assert 'role_matches = (' in stop
assert 'if role == "supervisor" and "writer" not in role_entries' in stop
assert 'selected supervisor disappeared without durable TERM/thaw progress' in stop
assert 'selected supervisor thaw is not journal-authorized' in stop
assert 'detached parent scope reactivated after durable terminal state' in stop
assert '"exit_cause": "unknown"' in node

spec = importlib.util.spec_from_file_location("recovery_freeze_contract", module_path)
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
assert module.FREEZE_PLAN_SCHEMA == "arc.recovery.freeze-plan.v5"
assert module.DEFAULT_ALLOW_MARKER_PATH == "/etc/arc-recovery/legacy-start-allowed"
assert module.FailClosedHostMutationAdapter
assert module.OFFLINE_RECONCILIATION_SCHEMA.endswith(".v1")
PY
}
classification_requires_each_node_once() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local r; r="$(printf '%s\n' '{"node":"nyc","classification":"valid_noncanonical_fork"}' '{"node":"lax","classification":"valid_noncanonical_fork"}' '{"node":"ams","classification":"valid_noncanonical_fork"}' '{"node":"lhr","classification":"preserved_unclassified"}' '{"node":"nrt","classification":"preserved_unclassified"}' '{"node":"sgp","classification":"preserved_unclassified"}' | summarize_binding_statuses)" || return 1
    [ "$r" = '0 3 3' ]
)

capture_readiness_resumes_stopped_and_indexed_nodes() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    # Fixture override invoked indirectly by sourced remote_readiness.
    # ShellCheck cannot see the indirect calls made by sourced remote_readiness.
    # shellcheck disable=SC2317,SC2329
    host_for() { printf '%s\n' "$1"; }
    # Fixture override invoked indirectly by sourced remote_readiness.
    # shellcheck disable=SC2317,SC2329
    freeze_node_field() {
        case "$3" in
            writer_pid|writer_start_ticks|supervisor_main_pid|supervisor_start_ticks|stake) printf '1\n' ;;
            boot_id) printf '00000000-0000-0000-0000-000000000000\n' ;;
            writer_supervision_mode) printf 'systemd-unit\n' ;;
            supervisor_unit) printf 'arc-node.service\n' ;;
            executable_path|supervisor_executable_path|data_dir|model_path) printf '/safe/%s/%s\n' "$2" "$3" ;;
            executable_sha256|argv_sha256|writer_cgroup_sha256|supervisor_executable_sha256|supervisor_argv_sha256|model_sha256|validator_address) printf 'a%.0s' {1..64}; printf '\n' ;;
            model_size_bytes) printf '4081004224\n' ;;
            *) return 1 ;;
        esac
    }
    # Fixture override invoked indirectly by sourced remote_readiness.
    # shellcheck disable=SC2317,SC2329
    ssh() {
        local joined="$*" node
        for node in nyc lax ams lhr nrt sgp; do case "$joined" in *"root@$node"*) break;; esac; done
        if { [ "$node" = nyc ] || [ "$node" = lax ]; } && [ ! -e "$f/$node-live-attempted" ]; then
            : > "$f/$node-live-attempted"
            return 1
        fi
        return 0
    }
    # Fixture override invoked indirectly by sourced remote_readiness.
    # shellcheck disable=SC2317,SC2329
    run_remote() {
        printf '%s %s\n' "$1" "$2" >> "$f/actions"
        case "$2:$1" in
            stopped-status:*) printf '{"stopped":true}\n' ;;
            status:nyc) printf '{"content_sealed":true}\n' ;;
            status:*) return 1 ;;
            *) return 1 ;;
        esac
    }
    remote_readiness "$(printf 'b%.0s' {1..64})" "$(printf 'a%.0s' {1..64})" /sealed/freeze.json >/dev/null || return 1
    grep -Fq 'nyc stopped-status' "$f/actions" && grep -Fq 'nyc status' "$f/actions" && \
        grep -Fq 'lax stopped-status' "$f/actions" && grep -Fq 'lax status' "$f/actions"
)

stale_freeze_capacity_cannot_cross_current_readiness_gate() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f gib sealed_data_bytes current_data_bytes available_bytes
    local sealed_required_bytes current_required_bytes capture_id freeze_sha
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    gib=$((1024 * 1024 * 1024))
    sealed_data_bytes=$((28 * gib))
    current_data_bytes=$((29 * gib))
    available_bytes=$((59 * gib))
    sealed_required_bytes=$((sealed_data_bytes * 2 + 2 * gib))
    current_required_bytes=$((current_data_bytes * 2 + 2 * gib))
    [ "$available_bytes" -ge "$sealed_required_bytes" ] || return 1
    [ "$available_bytes" -lt "$current_required_bytes" ] || return 1
    capture_id="$(printf 'b%.0s' {1..64})"
    freeze_sha="$(printf 'a%.0s' {1..64})"

    # Fixture overrides invoked indirectly by sourced remote_readiness.
    # shellcheck disable=SC2317,SC2329
    host_for() { printf '%s\n' "$1"; }
    # shellcheck disable=SC2317,SC2329
    freeze_node_field() {
        case "$3" in
            writer_pid|writer_start_ticks|supervisor_main_pid|supervisor_start_ticks) printf '1\n' ;;
            boot_id) printf '00000000-0000-0000-0000-000000000000\n' ;;
            writer_supervision_mode) printf 'systemd-unit\n' ;;
            supervisor_unit) printf 'arc-node.service\n' ;;
            executable_path|supervisor_executable_path|data_dir|model_path) printf '/safe/%s/%s\n' "$2" "$3" ;;
            executable_sha256|argv_sha256|writer_cgroup_sha256|supervisor_executable_sha256|supervisor_argv_sha256|model_sha256) printf 'a%.0s' {1..64}; printf '\n' ;;
            model_size_bytes) printf '4081004224\n' ;;
            *) return 1 ;;
        esac
    }
    # Model the remote current-capacity probe after the sealed plan passed:
    # data growth raises the live requirement from 58 GiB to 60 GiB.
    # shellcheck disable=SC2317,SC2329
    ssh_remote_exact() {
        local host="$1" command
        shift
        command="$*"
        [ "$host" = nyc ] || return 0
        [[ "$command" == *'bytes=$(du -s -B1 "$data"'* ]] || return 97
        [[ "$command" == *'required_bytes=$((bytes + binding_bytes))'* ]] || return 97
        [[ "$command" == *'free_bytes=$(df -PB1 /root'* ]] || return 97
        [[ "$command" == *'test "$free_bytes" -ge "$required_bytes"'* ]] || return 97
        : > "$f/nyc-current-capacity-probed"
        [ "$available_bytes" -ge "$current_required_bytes" ]
    }
    # shellcheck disable=SC2317,SC2329
    run_stopped_status_exact() {
        : > "$f/nyc-stopped-fallback-checked"
        return 1
    }

    if ( remote_readiness "$capture_id" "$freeze_sha" /sealed/stale-freeze.json \
        >/dev/null 2>&1; : > "$f/quarantine-mutation-reached" ); then
        return 1
    fi
    [ -e "$f/nyc-current-capacity-probed" ] && \
        [ -e "$f/nyc-stopped-fallback-checked" ] && \
        [ ! -e "$f/quarantine-mutation-reached" ]
)

fleet_live_observation_retry_rejects_any_stopped_writer() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    # shellcheck disable=SC2317,SC2329
    assert_pinned_freeze_bytes() { :; }
    # shellcheck disable=SC2317,SC2329
    run_remote() {
        if [ "$2" = live-observations-status ] && [ "$1" = nyc ]; then
            printf '{"complete":true}\n'
            return 0
        fi
        return 1
    }
    # shellcheck disable=SC2317,SC2329
    run_live_observations_eligibility_exact() {
        [ "$5" != nyc ]
    }
    # shellcheck disable=SC2317,SC2329
    verify_live_observation_generation_receipt_exact() {
        :
    }
    # shellcheck disable=SC2317,SC2329
    run_live_observations_exact() {
        : > "$f/network-recapture-attempted"
    }
    if ( capture_all_live_observations /sealed/freeze "$(printf 'a%.0s' {1..64})" \
            "$(printf 'b%.0s' {1..64})" "$(printf 'c%.0s' {1..64})" \
            /sealed/generation.json "$(printf 'd%.0s' {1..64})" \
            "$(printf 'e%.0s' {1..64})" "$f" "$f/statuses" ) >/dev/null 2>&1; then
        return 1
    fi
    [ ! -e "$f/network-recapture-attempted" ]
)

live_observation_selection_resume_is_byte_identical() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f generation freeze capture generation_file statuses selection first second
    f="$(mktemp -d)"; trap 'chmod -R u+w "$f" 2>/dev/null || true; rm -rf -- "$f"' EXIT
    generation="$(printf 'c%.0s' {1..64})"; freeze="$(printf 'a%.0s' {1..64})"
    capture="$(printf 'b%.0s' {1..64})"; generation_file="$f/$generation.json"
    statuses="$f/statuses.jsonl"; selection="$f/selection.json"
    python3 - "$generation_file" "$statuses" "$generation" "$freeze" "$capture" <<'PY' || return 1
import datetime,hashlib,json,pathlib,sys
generation_path,statuses_path=map(pathlib.Path,sys.argv[1:3]);generation,freeze,capture=sys.argv[3:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
now=datetime.datetime.now(datetime.timezone.utc)
stamp=lambda value:value.strftime("%Y-%m-%dT%H:%M:%S.%fZ")
drive={"schema":"arc.recovery.drive-prefreeze.v1","mode":"execute",
 "freeze_plan_sha256":freeze,"capture_id":capture,"remote_root_sha256":"1"*64,
 "client_id_sha256":"2"*64,"account_sha256":"3"*64,"permission_id_sha256":"4"*64,
 "rclone_version":"v1.75.0","source_bytes":1,"archive_reservation_bytes":2,
 "largest_object_reservation_bytes":1,"daily_upload_budget_bytes":2,
 "daily_upload_budget_basis":"operator-reviewed-remaining-dedicated-account",
 "available_bytes_before":8*1024*1024+2,"available_bytes_after":8*1024*1024+2,
 "canary_bytes":8*1024*1024,"canary_verified":True,"canary_deleted":True}
drive_sha=hashlib.sha256(canonical(drive)).hexdigest()
receipt={"schema":"arc.recovery.legacy-live-observation-generation.v1","source_main_commit":"9"*40,
    "freeze_plan_sha256":freeze,"capture_id":capture,"observation_generation":generation,
    "created_at":stamp(now-datetime.timedelta(seconds=2)),"max_selection_age_seconds":300,
    "drive_prefreeze_receipt":{"path":"/private/drive-prefreeze.json","sha256":drive_sha,"value":drive}}
generation_path.write_bytes(canonical(receipt));generation_path.chmod(0o400)
generation_sha=hashlib.sha256(canonical(receipt)).hexdigest()
rows=[]
for index,node in enumerate(("nyc","lax","ams","lhr","nrt","sgp")):
    rows.append({"schema":"arc.recovery.legacy-live-observations-status.v1","capture_id":capture,
        "observation_generation":generation,"observation_generation_receipt_sha256":generation_sha,
        "drive_prefreeze_receipt_sha256":drive_sha,"node":node,"freeze_plan_sha256":freeze,
        "created_at":stamp(now-datetime.timedelta(seconds=1)),"completed_at":stamp(now),
        "root_sha256":f"{index+1:064x}","receipt_sha256":f"{index+11:064x}",
        "labels":["diagnostic","noncanonical","nonreward"]})
statuses_path.write_bytes(b"".join(canonical(row) for row in rows));statuses_path.chmod(0o600)
PY
    first="$(seal_live_observation_selection "$selection" "$generation_file" "$statuses" \
        "$freeze" "$capture")" || return 1
    ln "$selection" "$selection.partial" || return 1
    read -r resume_state resume_generation < <(
        live_observation_selection_resume_state "$selection" "$f/no-rounds" \
            "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["drive_prefreeze_receipt"]["sha256"])' "$generation_file")" \
            "$freeze" "$capture"
    ) || return 1
    [ "$resume_state" = rotate ] && [ "$resume_generation" = "$generation" ] || return 1
    [ ! -e "$selection.partial" ] && \
        [ "$(stat -c %h "$selection" 2>/dev/null || stat -f %l "$selection")" -eq 1 ] || return 1

    # Crash before the create-only link leaves only a complete sealed partial.
    mv "$selection" "$selection.partial" || return 1
    read -r resume_state resume_generation < <(
        live_observation_selection_resume_state "$selection" "$f/no-rounds" \
            "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["drive_prefreeze_receipt"]["sha256"])' "$generation_file")" \
            "$freeze" "$capture"
    ) || return 1
    [ "$resume_state" = rotate ] && [ "$resume_generation" = "$generation" ] || return 1
    [ -f "$selection" ] && [ ! -e "$selection.partial" ] && \
        [ "$(stat -c %h "$selection" 2>/dev/null || stat -f %l "$selection")" -eq 1 ] || return 1
    second="$(seal_live_observation_selection "$selection" "$generation_file" "$statuses" \
        "$freeze" "$capture")" || return 1
    [ "$first" = "$second" ] && [ "$first" = "$(hash_file "$selection")" ] || return 1
    ln "$selection" "$selection.partial" || return 1
    [ "$(seal_live_observation_selection "$selection" "$generation_file" "$statuses" \
        "$freeze" "$capture")" = "$first" ] || return 1
    [ ! -e "$selection.partial" ] && [ "$(stat -c %h "$selection" 2>/dev/null || stat -f %l "$selection")" -eq 1 ] || return 1
    cp "$selection" "$f/selection-from-partial.json.partial";chmod 400 "$f/selection-from-partial.json.partial"
    [ "$(seal_live_observation_selection "$f/selection-from-partial.json" "$generation_file" "$statuses" \
        "$freeze" "$capture")" = "$first" ] || return 1
    cmp -s "$selection" "$f/selection-from-partial.json" || return 1
    printf '{"truncated"' > "$f/selection-from-truncated.json.partial";chmod 600 "$f/selection-from-truncated.json.partial"
    seal_live_observation_selection "$f/selection-from-truncated.json" "$generation_file" "$statuses" \
        "$freeze" "$capture" >/dev/null || return 1
    verify_live_observation_selection_exact "$selection" "$first" "$generation_file" \
        "$generation" "$(hash_file "$generation_file")" \
        "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["drive_prefreeze_receipt"]["sha256"])' "$generation_file")" \
        "$freeze" "$capture" || return 1
    ( verify_live_observation_selection_exact "$selection" "$first" "$generation_file" \
        "$(printf 'd%.0s' {1..64})" "$(hash_file "$generation_file")" \
        "$(printf 'e%.0s' {1..64})" "$freeze" "$capture" ) >/dev/null 2>&1 && return 1
    local authorization="$f/authorization.json" bad_authorization="$f/bad-authorization.json"
    python3 - "$selection" "$authorization" "$bad_authorization" <<'PY' || return 1
import json,pathlib,sys
selection=json.loads(pathlib.Path(sys.argv[1]).read_text())
value={"schema":"arc.recovery.quarantine-round-authorization.v1",
 "capture_id":selection["capture_id"],"freeze_plan_sha256":selection["freeze_plan_sha256"],
 "live_observation_selection_sha256":__import__("hashlib").sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest(),
 "live_observation_generation":selection["observation_generation"],
 "observation_generation_receipt_sha256":selection["observation_generation_receipt_sha256"],
 "drive_prefreeze_receipt_sha256":selection["drive_prefreeze_receipt_sha256"],
 "live_observation_selected_at":selection["selected_at"]}
canonical=lambda item:(json.dumps(item,sort_keys=True,separators=(",",":"))+"\n").encode()
pathlib.Path(sys.argv[2]).write_bytes(canonical(value));pathlib.Path(sys.argv[2]).chmod(0o400)
value["drive_prefreeze_receipt_sha256"]="e"*64
pathlib.Path(sys.argv[3]).write_bytes(canonical(value));pathlib.Path(sys.argv[3]).chmod(0o400)
PY
    quarantine_authorization_matches_live_observation "$authorization" "$selection" "$first" \
        "$generation_file" "$generation" "$(hash_file "$generation_file")" \
        "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["drive_prefreeze_receipt"]["sha256"])' "$generation_file")" \
        "$freeze" "$capture" || return 1
    ( quarantine_authorization_matches_live_observation "$bad_authorization" "$selection" "$first" \
        "$generation_file" "$generation" "$(hash_file "$generation_file")" \
        "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["drive_prefreeze_receipt"]["sha256"])' "$generation_file")" \
        "$freeze" "$capture" ) >/dev/null 2>&1 && return 1
    mkdir -m 700 "$f/selection-archive" "$f/truncated-archive"
    cp "$selection" "$f/truncated-selection.json";chmod 400 "$f/truncated-selection.json"
    printf '{"truncated"' > "$f/truncated-archive/.$generation.json.partial"
    chmod 600 "$f/truncated-archive/.$generation.json.partial"
    archive_stale_live_observation_selection "$f/truncated-selection.json" \
        "$f/truncated-archive" "$generation" || return 1
    [ ! -e "$f/truncated-selection.json" ] && \
        [ "$(hash_file "$f/truncated-archive/$generation.json")" = "$first" ] || return 1
    cp "$selection" "$f/selection-archive/$generation.json"
    chmod 400 "$f/selection-archive/$generation.json"
    ln "$f/selection-archive/$generation.json" \
        "$f/selection-archive/.$generation.json.partial"
    archive_stale_live_observation_selection "$selection" "$f/selection-archive" \
        "$generation" || return 1
    [ ! -e "$selection" ] && [ ! -e "$f/selection-archive/.$generation.json.partial" ] && \
        [ "$(hash_file "$f/selection-archive/$generation.json")" = "$first" ] || return 1
    return 0
)

mutation_dispatch_publication_is_no_replace_and_resumable() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f authorization readiness dispatch first second
    f="$(mktemp -d)"; trap 'chmod -R u+w "$f" 2>/dev/null || true; rm -rf -- "$f"' EXIT
    authorization="$f/authorization.json";readiness="$f/readiness.json";dispatch="$f/dispatch.json"
    python3 - "$authorization" "$readiness" <<'PY' || return 1
import hashlib,json,pathlib,sys
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
auth={"capture_id":"b"*64,"freeze_plan_sha256":"a"*64,"round_number":1,
 "live_observation_selection_sha256":"c"*64,"live_observation_generation":"d"*64,
 "observation_generation_receipt_sha256":"e"*64,"drive_prefreeze_receipt_sha256":"f"*64,
 "targets":[{"node":"nyc","host":"149.28.32.76"}]}
raw=canonical(auth);pathlib.Path(sys.argv[1]).write_bytes(raw);pathlib.Path(sys.argv[1]).chmod(0o400)
ready={"round_authorization_sha256":hashlib.sha256(raw).hexdigest(),"round_number":1}
pathlib.Path(sys.argv[2]).write_bytes(canonical(ready));pathlib.Path(sys.argv[2]).chmod(0o400)
PY
    first="$(seal_quarantine_mutation_dispatch "$authorization" "$readiness" "$dispatch")" || return 1
    ln "$dispatch" "$dispatch.partial" || return 1
    second="$(seal_quarantine_mutation_dispatch "$authorization" "$readiness" "$dispatch")" || return 1
    [ "$first" = "$second" ] && [ ! -e "$dispatch.partial" ] && \
        [ "$(stat -c %h "$dispatch" 2>/dev/null || stat -f %l "$dispatch")" -eq 1 ] || return 1
    cp "$dispatch" "$f/dispatch-from-partial.json.partial";chmod 400 "$f/dispatch-from-partial.json.partial"
    [ "$(seal_quarantine_mutation_dispatch "$authorization" "$readiness" \
        "$f/dispatch-from-partial.json")" = "$first" ] || return 1
    cmp -s "$dispatch" "$f/dispatch-from-partial.json" || return 1
    printf '{"truncated"' > "$f/dispatch-from-truncated.json.partial";chmod 600 "$f/dispatch-from-truncated.json.partial"
    seal_quarantine_mutation_dispatch "$authorization" "$readiness" \
        "$f/dispatch-from-truncated.json" >/dev/null || return 1
    python3 - "$ORCHESTRATOR" "$NODE_HELPER" <<'PY' || return 1
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text();node=pathlib.Path(sys.argv[2]).read_text()
selection=text[text.index("seal_live_observation_selection()"):
               text.index("verify_live_observation_generation_receipt_exact()")]
dispatch=text[text.index("seal_quarantine_mutation_dispatch()"):
              text.index("capture_post_quarantine_final_sources()")]
assert "os.rename(partial,output)" not in selection+dispatch
assert "os.link(partial,output,follow_symlinks=False)" in selection
assert "os.link(partial,output,follow_symlinks=False)" in dispatch
assert "recover_dynamic_partial" in node
assert "renameat2" in node
PY
)

readiness_without_dispatch_does_not_bind_stale_selection() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f attempt
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    attempt="$f/round-1/attempt.crash-before-dispatch"
    mkdir -p -- "$attempt/authorization-acceptances" \
        "$attempt/node-transitions"
    chmod 700 "$attempt/authorization-acceptances" \
        "$attempt/node-transitions"
    printf '{}\n' > "$attempt/authorization.json"
    printf '{}\n' > "$attempt/authorization-acceptances/nyc.json"
    printf '{}\n' > "$attempt/readiness.json"
    chmod 400 "$attempt/authorization.json" \
        "$attempt/authorization-acceptances/nyc.json" "$attempt/readiness.json"

    # Exact crash prefix: authorization and acceptance exist, local readiness
    # is durable, but dispatch was never published or sent. It must remain
    # powerless so an expired stale selection can rotate.
    if quarantine_attempt_binds_live_observation_selection "$attempt" \
            "$(printf 'a%.0s' {1..64})" "$(printf 'b%.0s' {1..64})"; then
        return 1
    fi

    printf '{}\n' > "$attempt/mutation-dispatch.json"
    chmod 400 "$attempt/mutation-dispatch.json"
    quarantine_attempt_binds_live_observation_selection "$attempt" \
        "$(printf 'a%.0s' {1..64})" "$(printf 'b%.0s' {1..64})" || return 1
    rm "$attempt/mutation-dispatch.json"

    printf '{}\n' > "$attempt/result.json"
    chmod 400 "$attempt/result.json"
    quarantine_attempt_binds_live_observation_selection "$attempt" \
        "$(printf 'a%.0s' {1..64})" "$(printf 'b%.0s' {1..64})" || return 1
    rm "$attempt/result.json"

    printf '{}\n' > "$attempt/node-transitions/nyc.json"
    chmod 400 "$attempt/node-transitions/nyc.json"
    quarantine_attempt_binds_live_observation_selection "$attempt" \
        "$(printf 'a%.0s' {1..64})" "$(printf 'b%.0s' {1..64})" || return 1

    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
scan=text[text.index("run_quarantine_generation_rounds()"):
          text.index("capture_phase()")]
assert 'quarantine_attempt_binds_live_observation_selection' in scan
assert '[ -e "$attempt_root/readiness.json" ]' not in scan
PY
)

local_create_only_post_link_crashes_are_reconciled_before_resume() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f selection maintenance rounds quarantine path
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    selection="$f/live-observation-selection.json"
    maintenance="$f/maintenance-inputs"
    rounds="$maintenance/quarantine-rounds"
    quarantine="$maintenance/network-quarantine"
    mkdir -p -- \
        "$rounds/round-1/attempt.crash/authorization-acceptances" \
        "$rounds/round-1/attempt.crash/node-transitions" "$quarantine"
    chmod 700 \
        "$rounds/round-1/attempt.crash/authorization-acceptances" \
        "$rounds/round-1/attempt.crash/node-transitions" "$quarantine"
    for path in \
        "$selection" \
        "$rounds/round-1/attempt.crash/authorization.json" \
        "$rounds/round-1/attempt.crash/readiness.json" \
        "$rounds/round-1/attempt.crash/mutation-dispatch.json" \
        "$rounds/round-1/attempt.crash/authorization-acceptances/nyc.json" \
        "$rounds/round-1/attempt.crash/node-transitions/nyc.json" \
        "$rounds/round-1/attempt.crash/result.json" \
        "$rounds/round-1/attempt.crash/zero-progress-release.json"; do
        printf '{}\n' > "$path";chmod 400 "$path";ln "$path" "$path.partial" || return 1
    done
    # A crash after sealing but before the no-replace link leaves only the
    # canonical mode-0400 partial. Dynamic readiness/result timestamps must be
    # recovered from these exact bytes rather than recomputed.
    for path in \
        "$rounds/round-1/attempt.crash/readiness-before-link.json" \
        "$rounds/round-1/attempt.crash/result-before-link.json"; do
        printf '{"completed_at":"2026-09-01T00:00:00Z"}\n' > "$path.partial"
        chmod 400 "$path.partial"
    done
    printf '{"completed_at":"2026-09-01T00:00:01Z"}\n' > \
        "$rounds/round-1/attempt.crash/readiness-before-fchmod.json.partial"
    chmod 600 "$rounds/round-1/attempt.crash/readiness-before-fchmod.json.partial"
    printf '{"completed_at"' > \
        "$rounds/round-1/attempt.crash/truncated-before-fchmod.json.partial"
    chmod 600 "$rounds/round-1/attempt.crash/truncated-before-fchmod.json.partial"
    # Dynamic sibling proofs are sampled again when their final is absent. A
    # sealed crash-before-link partial must therefore be promoted before any
    # fresh timestamp/counter bytes can be collected.
    for path in external-proof fleet-stability-proof; do
        printf '{"completed_at":"2026-09-01T00:00:02Z","proof":"%s"}\n' "$path" > \
            "$quarantine/$path.json.partial"
        chmod 400 "$quarantine/$path.json.partial"
    done
    reconcile_local_create_only_resume_links "$selection" "$maintenance" || return 1
    for path in \
        "$selection" \
        "$rounds/round-1/attempt.crash/authorization.json" \
        "$rounds/round-1/attempt.crash/readiness.json" \
        "$rounds/round-1/attempt.crash/mutation-dispatch.json" \
        "$rounds/round-1/attempt.crash/authorization-acceptances/nyc.json" \
        "$rounds/round-1/attempt.crash/node-transitions/nyc.json" \
        "$rounds/round-1/attempt.crash/result.json" \
        "$rounds/round-1/attempt.crash/zero-progress-release.json" \
        "$rounds/round-1/attempt.crash/readiness-before-link.json" \
        "$rounds/round-1/attempt.crash/result-before-link.json" \
        "$rounds/round-1/attempt.crash/readiness-before-fchmod.json" \
        "$quarantine/external-proof.json" \
        "$quarantine/fleet-stability-proof.json"; do
        [ -f "$path" ] && [ ! -e "$path.partial" ] && \
            [ "$(stat -c %h "$path" 2>/dev/null || stat -f %l "$path")" -eq 1 ] || return 1
    done
    [ ! -e "$rounds/round-1/attempt.crash/truncated-before-fchmod.json" ] && \
        [ ! -e "$rounds/round-1/attempt.crash/truncated-before-fchmod.json.partial" ] || return 1
)

positive_round_result_resume_is_byte_identical() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f selection maintenance rounds attempt result first
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    selection="$f/live-observation-selection.json"
    maintenance="$f/maintenance-inputs"
    rounds="$maintenance/quarantine-rounds"
    attempt="$rounds/round-1/attempt.positive"
    result="$attempt/result.json"
    mkdir -p -- "$attempt/node-transitions"
    chmod 700 "$attempt/node-transitions"
    first="$(PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - \
        "$attempt" "$rounds" <<'PY'
import pathlib,sys,types
import quarantine_round_driver as driver
import test_quarantine_rounds as fixture

attempt=pathlib.Path(sys.argv[1]);rounds=pathlib.Path(sys.argv[2])
canonical=driver.canonical
names=[name for name,_host in fixture.qr.FLEET]
authorization=fixture.authorization(1,[],names,0)
readiness=fixture.target_readiness(authorization)
dispatch=fixture.mutation_dispatch(authorization,readiness)
transitions=[
    fixture.applied(
        authorization,name,20+index,fixture.authorized_height(authorization,name)
    )
    for index,name in enumerate(names)
]
for path,value in (
    (attempt/"authorization.json",authorization),
    (attempt/"readiness.json",readiness),
    (attempt/"mutation-dispatch.json",dispatch),
):
    path.write_bytes(canonical(value));path.chmod(0o400)
for transition in transitions:
    path=attempt/f"node-transitions/{transition['node']}.json"
    path.write_bytes(canonical(transition));path.chmod(0o400)
args=types.SimpleNamespace(
    authorization=attempt/"authorization.json",readiness=attempt/"readiness.json",
    dispatch=attempt/"mutation-dispatch.json",remaining_proof_root=None,
    round_root=rounds,round_number=1,applied_root=attempt/"node-transitions",
    output=attempt/"result.json",
)
driver.utc_now=lambda:fixture.utc(330)
value=driver.build_result(args)
driver.publish(args.output,value,"round result")
print(driver.digest_bytes(args.output.read_bytes()))
PY
    )" || return 1
    [ -n "$first" ] || return 1

    # Crash after the no-replace link but before partial unlink.
    ln "$result" "$result.partial" || return 1
    reconcile_local_create_only_resume_links "$selection" "$maintenance" || return 1
    PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - "$attempt" "$rounds" "$first" <<'PY' || return 1
import pathlib,sys,types
import quarantine_round_driver as driver
import test_quarantine_rounds as fixture
attempt=pathlib.Path(sys.argv[1]);rounds=pathlib.Path(sys.argv[2]);expected=sys.argv[3]
args=types.SimpleNamespace(
    authorization=attempt/"authorization.json",readiness=attempt/"readiness.json",
    dispatch=attempt/"mutation-dispatch.json",remaining_proof_root=None,
    round_root=rounds,round_number=1,applied_root=attempt/"node-transitions",
    output=attempt/"result.json",
)
driver.utc_now=lambda:fixture.utc(600)
value=driver.build_result(args)
assert driver.digest_bytes(driver.canonical(value))==expected
driver.publish(args.output,value,"round result")
assert driver.digest_bytes(args.output.read_bytes())==expected
PY

    # Crash after sealing/fsync but before the no-replace link.
    mv "$result" "$result.partial" || return 1
    reconcile_local_create_only_resume_links "$selection" "$maintenance" || return 1
    PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - "$attempt" "$rounds" "$first" <<'PY' || return 1
import pathlib,sys,types
import quarantine_round_driver as driver
import test_quarantine_rounds as fixture
attempt=pathlib.Path(sys.argv[1]);rounds=pathlib.Path(sys.argv[2]);expected=sys.argv[3]
args=types.SimpleNamespace(
    authorization=attempt/"authorization.json",readiness=attempt/"readiness.json",
    dispatch=attempt/"mutation-dispatch.json",remaining_proof_root=None,
    round_root=rounds,round_number=1,applied_root=attempt/"node-transitions",
    output=attempt/"result.json",
)
driver.utc_now=lambda:fixture.utc(900)
value=driver.build_result(args)
assert driver.digest_bytes(driver.canonical(value))==expected
assert driver.digest_bytes(args.output.read_bytes())==expected
assert not args.output.with_name(args.output.name+".partial").exists()
PY
)

positive_partial_waits_for_late_transition_before_sealing() (
    local f rounds attempt
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    rounds="$f/quarantine-rounds"
    attempt="$rounds/round-1/attempt.late-sixth"
    mkdir -p -- "$attempt/node-transitions"
    chmod 700 "$attempt/node-transitions"
    PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - \
        "$attempt" "$rounds" <<'PY' || return 1
import pathlib
import types
import quarantine_round_driver as driver
import test_quarantine_rounds as fixture

attempt = pathlib.Path(__import__("sys").argv[1])
rounds = pathlib.Path(__import__("sys").argv[2])
for path in (rounds, rounds / "round-1", attempt, attempt / "node-transitions"):
    path.chmod(0o700)
names = [name for name, _host in fixture.qr.FLEET]
authorization = fixture.authorization(1, [], names, 0)
readiness = fixture.target_readiness(authorization)
dispatch = fixture.mutation_dispatch(authorization, readiness)
for path, value in (
    (attempt / "authorization.json", authorization),
    (attempt / "readiness.json", readiness),
    (attempt / "mutation-dispatch.json", dispatch),
):
    path.write_bytes(driver.canonical(value))
    path.chmod(0o400)
for index, name in enumerate(names[:5]):
    value = fixture.applied(
        authorization, name, 20 + index,
        fixture.authorized_height(authorization, name),
    )
    path = attempt / "node-transitions" / f"{name}.json"
    path.write_bytes(driver.canonical(value))
    path.chmod(0o400)

args = types.SimpleNamespace(
    authorization=attempt / "authorization.json",
    readiness=attempt / "readiness.json",
    dispatch=attempt / "mutation-dispatch.json",
    round_root=rounds,
    round_number=1,
    applied_root=attempt / "node-transitions",
    remaining_proof_root=None,
    output=attempt / "result.json",
)
driver.utc_now = lambda: fixture.utc(330)
try:
    driver.build_result(args)
except driver.DriverError as error:
    assert "inert-proof root" in str(error)
else:
    raise AssertionError("five transitions sealed while the sixth remained ambiguous")
assert not args.output.exists() and not args.output.with_name("result.json.partial").exists()

# A late applied-status receipt arrives while no terminal result exists.  The
# exact current closure must become all six nodes and therefore need no inert
# proof; this is the recoverable side of the proof/apply lock ordering race.
late = fixture.applied(
    authorization, names[5], 25,
    fixture.authorized_height(authorization, names[5]),
)
late_path = attempt / "node-transitions" / f"{names[5]}.json"
late_path.write_bytes(driver.canonical(late))
late_path.chmod(0o400)
value = driver.build_result(args)
assert value["remaining_targets"] == []
assert value["remaining_target_inert_proofs"] == []
driver.publish(args.output, value, "round result")
sealed_sha = driver.digest_bytes(args.output.read_bytes())

# Simulate the operator crash after the attempt result became durable but
# before it was copied into the immutable prefix.  A later wall time must not
# change the terminal attempt bytes or prevent prefix completion.
driver.utc_now = lambda: fixture.utc(900)
resumed = driver.build_result(args)
assert driver.digest_bytes(driver.canonical(resumed)) == sealed_sha
final = rounds / "round-1"
driver.publish(final / "authorization.json", authorization, "round authorization")
driver.publish(final / "result.json", resumed, "round result")
ledger = driver.build_ledger(types.SimpleNamespace(
    round_root=rounds,
    capture_id=fixture.CAPTURE,
    freeze_plan_sha256=fixture.FREEZE,
))
assert len(ledger["rounds"]) == 1
assert [
    wrapper["value"]["node"]
    for wrapper in ledger["rounds"][0]["result"]["value"]["transitions"]
] == names
PY
    python3 - "$ORCHESTRATOR" "$NODE_HELPER" <<'PY' || return 1
import pathlib
import sys
orchestrator = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
node = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")
capture = orchestrator[
    orchestrator.index("capture_remaining_target_inert_proofs()"):
    orchestrator.index("archive_stale_live_observation_selection()")
]
complete = orchestrator[
    orchestrator.index("complete_quarantine_round_attempt()"):
    orchestrator.index("run_quarantine_generation_rounds()")
]
proof = node[
    node.index("quarantine_round_zero_progress_proof()"):
    node.index("legacy_height_bracket()")
]
assert "quarantine-round-zero-progress-proof" in capture
assert 'exec 5<> "$attempt_root/round.lock"' in proof
assert "time.CLOCK_BOOTTIME" in proof
assert "elapsed<=300_000_000_000" in proof
assert "capture_remaining_target_inert_proofs" in complete
assert "return 4" in complete
PY
)

sealed_partial_resume_skips_old_attempt_status() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f rounds attempt log_root marker sealed_sha freeze capture
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    rounds="$f/quarantine-rounds"
    attempt="$rounds/round-1/attempt.partial"
    log_root="$f/logs"
    marker="$f/old-attempt-status-called"
    mkdir -p -- "$attempt/node-transitions" "$log_root"
    chmod 700 "$attempt/node-transitions" "$log_root"
    read -r freeze capture sealed_sha < <(
        PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - \
            "$attempt" "$rounds" "$f/late-sixth.json" <<'PY'
import pathlib
import sys
import types
import quarantine_round_driver as driver
import test_quarantine_rounds as fixture

attempt, rounds, late_path = map(pathlib.Path, sys.argv[1:])
for path in (rounds, rounds / "round-1", attempt, attempt / "node-transitions"):
    path.chmod(0o700)
names = [name for name, _host in fixture.qr.FLEET]
authorization = fixture.authorization(1, [], names, 0)
readiness = fixture.target_readiness(authorization)
dispatch = fixture.mutation_dispatch(authorization, readiness)
for path, value in (
    (attempt / "authorization.json", authorization),
    (attempt / "readiness.json", readiness),
    (attempt / "mutation-dispatch.json", dispatch),
):
    path.write_bytes(driver.canonical(value))
    path.chmod(0o400)
for index, name in enumerate(names[:5]):
    value = fixture.applied(
        authorization, name, 20 + index,
        fixture.authorized_height(authorization, name),
    )
    path = attempt / "node-transitions" / f"{name}.json"
    path.write_bytes(driver.canonical(value))
    path.chmod(0o400)
proof_root = attempt / f"remaining-target-inert-proofs-{fixture.H['96']}"
proof_root.mkdir(mode=0o700)
proof = fixture.remaining_target_inert_proof(
    authorization, readiness, dispatch, names[5]
)
proof_path = proof_root / f"{names[5]}.json"
proof_path.write_bytes(driver.canonical(proof))
proof_path.chmod(0o400)
args = types.SimpleNamespace(
    authorization=attempt / "authorization.json",
    readiness=attempt / "readiness.json",
    dispatch=attempt / "mutation-dispatch.json",
    round_root=rounds,
    round_number=1,
    applied_root=attempt / "node-transitions",
    remaining_proof_root=proof_root,
    output=attempt / "result.json",
)
driver.utc_now = lambda: fixture.utc(330)
driver.publish(args.output, driver.build_result(args), "round result")
late = fixture.applied(
    authorization, names[5], 25,
    fixture.authorized_height(authorization, names[5]),
)
late_path.write_bytes(driver.canonical(late))
late_path.chmod(0o400)
print(
    fixture.FREEZE,
    fixture.CAPTURE,
    driver.digest_bytes(args.output.read_bytes()),
)
PY
    ) || return 1
    [ -n "$sealed_sha" ] || return 1
    quarantine_authorization_matches_live_observation() { return 0; }
    run_remote() {
        case "${2:-}" in
            quarantine-round-applied-status|quarantine-round-stopped-precommit)
                : > "$marker"
                cat "$f/late-sixth.json"
                return 0
                ;;
        esac
        return 1
    }
    complete_quarantine_round_attempt \
        "$f/freeze-plan.json" "$freeze" "$capture" 1 "$rounds" "$attempt" \
        "$log_root" "$(printf '1%.0s' {1..64})" "$(printf '2%.0s' {1..64})" \
        "$(printf '3%.0s' {1..64})" "$(printf '4%.0s' {1..64})" 0 \
        "$f/selection.json" "$(printf '5%.0s' {1..64})" \
        "$f/generation.json" "$(printf '6%.0s' {1..64})" \
        "$(printf '7%.0s' {1..64})" "$(printf '8%.0s' {1..64})" 1 1 || return 1
    [ ! -e "$marker" ] || return 1
    [ "$(hash_file "$attempt/result.json")" = "$sealed_sha" ] || return 1
    cmp --silent "$attempt/result.json" "$rounds/round-1/result.json" || return 1
    cmp --silent "$attempt/authorization.json" \
        "$rounds/round-1/authorization.json" || return 1
)

zero_progress_result_never_enters_immutable_prefix() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f rounds attempt log_root marker freeze capture status
    f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    rounds="$f/quarantine-rounds"
    attempt="$rounds/round-1/attempt.zero"
    log_root="$f/logs"
    marker="$f/remote-called"
    mkdir -p -- "$attempt/node-transitions" "$log_root"
    chmod 700 "$attempt/node-transitions" "$log_root"
    read -r freeze capture < <(
        PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - \
            "$attempt" "$rounds" <<'PY'
import pathlib
import sys
import quarantine_round_driver as driver
import test_quarantine_rounds as fixture

attempt, rounds = map(pathlib.Path, sys.argv[1:])
for path in (rounds, rounds / "round-1", attempt, attempt / "node-transitions"):
    path.chmod(0o700)
names = [name for name, _host in fixture.qr.FLEET]
authorization = fixture.authorization(1, [], names, 0)
readiness = fixture.target_readiness(authorization)
dispatch = fixture.mutation_dispatch(authorization, readiness)
result = fixture.result(authorization, [], 303)
for path, value in (
    (attempt / "authorization.json", authorization),
    (attempt / "readiness.json", readiness),
    (attempt / "mutation-dispatch.json", dispatch),
    (attempt / "result.json", result),
):
    path.write_bytes(driver.canonical(value))
    path.chmod(0o400)
print(fixture.FREEZE, fixture.CAPTURE)
PY
    ) || return 1
    quarantine_authorization_matches_live_observation() { return 0; }
    run_remote() { : > "$marker"; return 1; }
    set +e
    complete_quarantine_round_attempt \
        "$f/freeze-plan.json" "$freeze" "$capture" 1 "$rounds" "$attempt" \
        "$log_root" "$(printf '1%.0s' {1..64})" "$(printf '2%.0s' {1..64})" \
        "$(printf '3%.0s' {1..64})" "$(printf '4%.0s' {1..64})" 0 \
        "$f/selection.json" "$(printf '5%.0s' {1..64})" \
        "$f/generation.json" "$(printf '6%.0s' {1..64})" \
        "$(printf '7%.0s' {1..64})" "$(printf '8%.0s' {1..64})" 1 1
    status=$?
    set -e
    [ "$status" -eq 2 ] || return 1
    [ ! -e "$marker" ] || return 1
    [ ! -e "$rounds/round-1/authorization.json" ] || return 1
    [ ! -e "$rounds/round-1/result.json" ] || return 1
)

released_zero_progress_attempt_does_not_bind_rotated_selection() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f attempt freeze capture
    f="$(mktemp -d)";trap 'rm -rf -- "$f"' EXIT
    attempt="$f/round-1/attempt.released";mkdir -p -- "$attempt/node-transitions"
    chmod 700 "$attempt/node-transitions"
    freeze="$(printf '0%.0s' {1..63})2";capture="$(printf '0%.0s' {1..63})1"
    PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - "$attempt" <<'PY' || return 1
import datetime,hashlib,json,pathlib,sys
import test_quarantine_rounds as fixture
root=pathlib.Path(sys.argv[1]);qr=fixture.qr
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda value:hashlib.sha256(canonical(value)).hexdigest()
names=[name for name,_host in qr.FLEET]
authorization=fixture.authorization(1,[],names,0)
readiness=fixture.target_readiness(authorization)
auth_sha=digest(authorization);readiness_sha=digest(readiness)
dispatch={"schema":"arc.recovery.quarantine-mutation-dispatch.v1",
 "capture_id":fixture.CAPTURE,"freeze_plan_sha256":fixture.FREEZE,"round_number":1,
 "round_authorization_sha256":auth_sha,"round_readiness_sha256":readiness_sha,
 "live_observation_selection_sha256":authorization["live_observation_selection_sha256"],
 "live_observation_generation":authorization["live_observation_generation"],
 "observation_generation_receipt_sha256":authorization["observation_generation_receipt_sha256"],
 "drive_prefreeze_receipt_sha256":authorization["drive_prefreeze_receipt_sha256"],
 "targets":[{"node":row["node"],"host":row["host"]} for row in authorization["targets"]],
 "dispatched_at":fixture.utc(7)}
dispatch_sha=digest(dispatch);challenge="5"*64
proofs=[]
for row in authorization["targets"]:
    proof={"schema":"arc.recovery.quarantine-round-zero-progress-node-proof.v1",
      "capture_id":fixture.CAPTURE,"freeze_plan_sha256":fixture.FREEZE,
      "observation_generation":authorization["live_observation_generation"],
      "round_number":1,"round_authorization_sha256":auth_sha,
      "round_readiness_sha256":readiness_sha,"mutation_dispatch_sha256":dispatch_sha,
      "challenge":challenge,"node":row["node"],"boot_id":row["boot_id"],
      "writer_live_unfenced":True,"apply_state_present":False,
      "restart_effective_mutation_absent":True,"active_selector_absent":True,
      "quarantine_nft_absent":True,"authorization_accepted":True,
      "readiness_present":False,"accepted_boottime_ns":1_000_000_000,
      "elapsed_since_acceptance_ns":300_000_000_001,
      "observed_boottime_ns":301_000_000_001,"observed_at":fixture.utc(400)}
    proofs.append({"value":proof,"sha256":digest(proof)})
release={"schema":"arc.recovery.quarantine-round-zero-progress-release.v1",
 "capture_id":fixture.CAPTURE,"freeze_plan_sha256":fixture.FREEZE,"round_number":1,
 "round_authorization_sha256":auth_sha,"round_readiness_sha256":readiness_sha,
 "mutation_dispatch_sha256":dispatch_sha,
 "live_observation_selection_sha256":authorization["live_observation_selection_sha256"],
 "live_observation_generation":authorization["live_observation_generation"],
 "observation_generation_receipt_sha256":authorization["observation_generation_receipt_sha256"],
 "drive_prefreeze_receipt_sha256":authorization["drive_prefreeze_receipt_sha256"],
 "challenge":challenge,"released_at":fixture.utc(401),"nodes":proofs}
result={"schema":qr.ROUND_RESULT_SCHEMA,"capture_id":fixture.CAPTURE,
 "freeze_plan_sha256":fixture.FREEZE,"round_number":1,
 "round_authorization_sha256":auth_sha,"target_readiness":qr.wrap(readiness),
 "transitions":[],"mutation_dispatch":qr.wrap(dispatch),
 "remaining_target_inert_proofs":[],"remaining_targets":names,
 "completed_at":authorization["authorization_deadline"]}
for name,value in (("authorization.json",authorization),("readiness.json",readiness),
                   ("mutation-dispatch.json",dispatch),("result.json",result),
                   ("zero-progress-release.json",release)):
    path=root/name;path.write_bytes(canonical(value));path.chmod(0o400)
PY
    # This is the post-release, post-rotation scan of the old attempt.  The
    # exact release makes its readiness/dispatch/result nonbinding.
    if quarantine_attempt_binds_live_observation_selection "$attempt" \
            "$freeze" "$capture"; then
        return 1
    fi
    chmod 600 "$attempt/zero-progress-release.json"
    set +e
    quarantine_attempt_has_valid_zero_progress_release "$attempt" \
        "$freeze" "$capture" >/dev/null 2>&1
    local invalid_status=$?
    set -e
    [ "$invalid_status" -ne 0 ] || return 1
    chmod 400 "$attempt/zero-progress-release.json"
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
scan=text[text.index("run_quarantine_generation_rounds()"):
          text.index("capture_phase()")]
assert 'quarantine_attempt_binds_live_observation_selection' in scan
assert 'quarantine_attempt_has_valid_zero_progress_release' in text
PY
)

remote_zero_progress_heals_only_reviewed_publish_orphans() (
    python3 - "$NODE_HELPER" <<'PY' || return 1
import os,pathlib,re,stat,sys,tempfile
text=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
proof=text[text.index("quarantine_round_zero_progress_proof()"):
           text.index("legacy_height_bracket()")]
assert proof.index("partial_re=") < proof.index("for member in state.iterdir():")
for exact in (
    "authorization\\.json|readiness\\.json|policy\\.nft|apply|nft|",
    "table-binding\\.json|nft-apply-intent\\.json|persistence-plan\\.json|",
    "item.st_size<0 or item.st_size>16*1024*1024",
    "stat.S_IMODE(item.st_mode) not in {0o600,expected_mode}",
    "os.unlink(member);cleaned=True",
):
    assert exact in proof, exact

# Crash-before-rename model for the exact reviewed random temporary namespace.
allowed={"authorization.json":0o400,"readiness.json":0o400,"policy.nft":0o400,
         "apply":0o500,"nft":0o500,"table-binding.json":0o400,
         "nft-apply-intent.json":0o400,"persistence-plan.json":0o400,
         "contract.json":0o400}
pattern=re.compile(r"^\.(authorization\.json|readiness\.json|policy\.nft|apply|nft|"
                   r"table-binding\.json|nft-apply-intent\.json|persistence-plan\.json|"
                   r"contract\.json)\.([1-9][0-9]*)\.([0-9a-f]{16})\.partial$")
with tempfile.TemporaryDirectory() as raw:
    root=pathlib.Path(raw)
    safe=[]
    for index,(name,mode) in enumerate(allowed.items(),1):
        path=root/f".{name}.{index}.0123456789abcdef.partial"
        path.write_bytes(b"partial");path.chmod(0o600 if index%2 else mode);safe.append(path)
    hostile=root/".unreviewed.json.1.0123456789abcdef.partial"
    hostile.write_bytes(b"hostile");hostile.chmod(0o600)
    for member in list(root.iterdir()):
        match=pattern.fullmatch(member.name)
        if match is None:continue
        info=member.lstat();expected=allowed[match.group(1)]
        assert stat.S_ISREG(info.st_mode) and stat.S_IMODE(info.st_mode) in {0o600,expected}
        member.unlink()
    assert all(not path.exists() for path in safe) and hostile.exists()
PY
)

capture_lock_and_monotonic_lease_are_portable_and_bound() (
    local lock_root;lock_root="$(python3 - <<'PY'
import os,pathlib,stat
tmp=pathlib.Path(os.path.realpath("/tmp"));assert tmp.is_dir() and tmp.stat().st_uid==0
root=tmp/f"arc-recovery-lock-smoke-{os.geteuid()}-{os.getpid()}";root.mkdir(mode=0o700)
print(root)
PY
)" || return 1
    trap 'exec 7<&- 2>/dev/null || true; rmdir "$lock_root" 2>/dev/null || true' EXIT
    exec 7<"$lock_root"
    python3 - 7 <<'PY' || return 1
import fcntl,sys
fcntl.flock(int(sys.argv[1]),fcntl.LOCK_EX|fcntl.LOCK_NB)
PY
    python3 - "$lock_root" <<'PY' || return 1
import errno,fcntl,os,sys
fd=os.open(sys.argv[1],os.O_RDONLY)
try:fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
except OSError as error:
    if error.errno in {errno.EACCES,errno.EAGAIN}:raise SystemExit(0)
    raise
raise SystemExit("second capture acquired the held lock")
PY
    python3 - "$ORCHESTRATOR" "$NODE_HELPER" "$REPO_ROOT/scripts/recovery/quarantine_rounds.py" <<'PY' || return 1
import pathlib,sys
fleet=pathlib.Path(sys.argv[1]).read_text();node=pathlib.Path(sys.argv[2]).read_text()
rounds=pathlib.Path(sys.argv[3]).read_text()
assert 'os.path.realpath(tmp_entry)' in fleet
assert 'exec 8<"$capture_state_lock_dir"' in fleet
assert 'fcntl.LOCK_EX|fcntl.LOCK_NB' in fleet
assert 'quarantine-round-zero-progress-proof' in fleet
assert 'arc.recovery.quarantine-round-zero-progress-release.v1' in fleet
assert 'set(value)!=proof_fields' in fleet and 'set(proof)!=proof_fields' in fleet
assert 'authorization_accepted") is not True' in fleet
assert 'observed_ns<=accepted_ns+300_000_000_000' in fleet
assert 'exec 5<> "$attempt_root/round.lock"' in node
assert 'time.clock_gettime_ns(time.CLOCK_BOOTTIME)' in node
assert 'time.monotonic_ns()' not in node[node.index('def validate_readiness'):node.index('def input_bytes')]
assert 'public_started <= public_completed <= cross_started' not in node
assert 'public_started <= public_completed <= cross_started' not in rounds
PY
    PYTHONPATH="$REPO_ROOT/scripts/recovery" python3 - <<'PY' || return 1
import quarantine_round_driver as driver
start_mono=10_000_000_000;start_wall=100_000_000_000
assert driver.operator_selection_remaining_ns(
    start_mono,start_wall,now_monotonic_ns=start_mono+1_000_000_000,
    now_realtime_ns=start_wall+1_000_000_000)==299_000_000_000
for now_mono,now_wall in (
    (start_mono+2_000_000_000,start_wall-1),       # wall-clock backstep
    (start_mono+2_000_000_000,start_wall+5_000_000_000), # suspend/divergence
    (start_mono+301_000_000_000,start_wall+301_000_000_000), # expiry
):
    try:
        driver.operator_selection_remaining_ns(
            start_mono,start_wall,now_monotonic_ns=now_mono,now_realtime_ns=now_wall
        )
    except driver.DriverError:pass
    else:raise AssertionError((now_mono,now_wall))
PY
)

reference_pair_is_independent_of_final_capture_classes() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f h; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT; h="$(printf '1%.0s' {1..64})"
    for name in genesis validators legacy snapshot wal; do printf '%s\n' "$name" > "$f/$name"; done
    cat > "$f/arc-node" <<'SH'
#!/bin/sh
out=
while [ "$#" -gt 0 ]; do if [ "$1" = --output ]; then out=$2; shift 2; else shift; fi; done
printf checkpoint > "$out"
height=137145; case "$0" in *-bad) height=137146;; esac
printf '{"status":"EXPORTED_UNSIGNED","source_height":%s,"source_block_hash":"%s","source_state_root":"%s","full_state_root":"%s","manifest_hash":"%s","source_consensus_round":7,"created_at_unix_ms":8,"recovery_epoch":9,"validator_set_id":10,"source_validator_count":8,"source_validator_stake":40000000,"source_validator_set_hash":"80d7c2d229fea4171732fd04451372d849fab7baefed143a2a445ae72f472ecd"}\n' "$height" "$(printf '1%.0s' $(seq 1 64))" "$(printf '2%.0s' $(seq 1 64))" "$(printf '3%.0s' $(seq 1 64))" "$(printf '4%.0s' $(seq 1 64))"
SH
    chmod 700 "$f/arc-node"
    if ! verify_reference_pair "$f/arc-node" "$f/genesis" "$f/validators" "$f/legacy" "$f/snapshot" "$f/wal" 7 8 9 10 137145 "$h" "$(printf '2%.0s' {1..64})" "$(printf '3%.0s' {1..64})" "$(printf '4%.0s' {1..64})" false >/dev/null; then
        printf 'good independent reference fixture was rejected\n' >&2
        return 1
    fi
    cp "$f/arc-node" "$f/arc-node-bad"; chmod 700 "$f/arc-node-bad"
    if ( verify_reference_pair "$f/arc-node-bad" "$f/genesis" "$f/validators" "$f/legacy" "$f/snapshot" "$f/wal" 7 8 9 10 137145 "$h" "$(printf '2%.0s' {1..64})" "$(printf '3%.0s' {1..64})" "$(printf '4%.0s' {1..64})" false ) >/dev/null 2>&1; then
        printf 'bad independent reference fixture was accepted\n' >&2
        return 1
    fi
    return 0
)

remote_complete_rejects_missing_tampered_extra() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f remote archive_sha complete_sha sums_sha freeze_sha rollout_sha; f="$(realpath "$(mktemp -d)")"; trap 'rm -rf -- "$f"' EXIT
    remote="$f/remote"; mkdir -p "$f/shared" "$f/meta" "$f/complete" "$remote" "$f/bin"; mkdir -m 700 "$f/catalog"; printf alpha > "$f/shared/a.txt"
    python3 - "$f" <<'PY' || return 1
import hashlib,json,pathlib,sys
r=pathlib.Path(sys.argv[1]); d=r/"remote"; s=r/"shared"; rows=[]; live_rows=[]; cs=["valid_noncanonical_fork"]*3+["preserved_unclassified"]*3
h=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for name in ("arc-node","genesis.toml","validator-public-keys.json","legacy-validator-set-40m.json","source.snapshot.lz4","source.state.wal","recovery.arcchkpt","archive-fleet-to-drive.sh","archive-node.sh","recovery_rollout.py","recovery-manifest.schema.json"):
 (s/name).write_bytes((name+"-bytes").encode())
(s/"freeze-plan.json").write_bytes(b"freeze-plan-bytes\n");freeze_sha=h(s/"freeze-plan.json")
(s/"rollout-manifest.json").write_bytes(b"rollout-manifest-bytes\n");rollout_sha=h(s/"rollout-manifest.json")
(s/"freeze-plan.json.sha256").write_text(f"{freeze_sha}  freeze-plan.json\n")
(s/"rollout-manifest.json.sha256").write_text(f"{rollout_sha}  rollout-manifest.json\n")
(s/"source-commit.txt").write_text("1"*40+"\n");(s/"capture-id.txt").write_text("b"*64+"\n")
obj=lambda name:{"name":name,"size":(s/name).stat().st_size,"sha256":h(s/name)}
ref={"schema":"arc.recovery.canonical-reference.v1","independently_verified":True,"allow_unbound_legacy_wal":False,"verifier_binary":obj("arc-node"),"genesis":obj("genesis.toml"),"validator_public_keys":obj("validator-public-keys.json"),"legacy_validator_set":obj("legacy-validator-set-40m.json"),"source_snapshot":obj("source.snapshot.lz4"),"source_wal":obj("source.state.wal"),"selected_checkpoint":obj("recovery.arcchkpt"),"source_height":137145,"source_block_hash":"1"*64,"source_state_root":"2"*64,"transition_state_root":"3"*64,"checkpoint_manifest_hash":"4"*64,"source_consensus_round":7,"created_at_unix_ms":8,"recovery_epoch":9,"validator_set_id":10}
(s/"canonical-reference.json").write_text(json.dumps(ref,sort_keys=True,separators=(",",":"))+"\n")
(s/"archive-seal-options.json").write_text('{"allow_unbound_legacy_wal":false}\n')
for n,c in zip(("nyc","lax","ams","lhr","nrt","sgp"),cs):
 root_hash=hashlib.sha256((n+"-observation-root").encode()).hexdigest(); receipt_hash=hashlib.sha256((n+"-observation-receipt").encode()).hexdigest()
 live_rows.append({"node":n,"root_sha256":root_hash,"receipt_sha256":receipt_hash})
 b=d/f"legacy-{n}.tar.zst"; b.write_bytes((n+"-bundle").encode()); i=d/f"legacy-{n}.inventory"; i.write_text(n+"-inventory\n"); bs=d/(b.name+".sha256"); bs.write_text(f"{h(b)}  {b.name}\n"); ins=d/(i.name+".sha256"); ins.write_text(f"{h(i)}  {i.name}\n"); rows.append({"schema":"arc.recovery.bundle-status.v1","capture_id":"b"*64,"node":n,"rollout_manifest_sha256":rollout_sha,"classification":c,"bundle":{"name":b.name,"size":b.stat().st_size,"sha256":h(b),"sidecar_name":bs.name,"sidecar_sha256":h(bs)},"inventory":{"name":i.name,"size":i.stat().st_size,"sha256":h(i),"sidecar_name":ins.name,"sidecar_sha256":h(ins)}})
observation={"legacy_live_observation_selection_sha256":"c"*64,
             "legacy_live_observation_generation":"d"*64,
             "observation_generation_receipt_sha256":"e"*64,
             "drive_prefreeze_receipt_sha256":"f"*64}
(s/"offline-stop-evidence.json").write_text(json.dumps(observation,sort_keys=True,separators=(",",":"))+"\n")
(s/"legacy-live-observations.json").write_text(json.dumps({"schema":"arc.recovery.legacy-live-observations-fleet.v1","capture_id":"b"*64,"freeze_plan_sha256":freeze_sha,"observation_generation":observation["legacy_live_observation_generation"],"observation_generation_receipt_sha256":observation["observation_generation_receipt_sha256"],"drive_prefreeze_receipt_sha256":observation["drive_prefreeze_receipt_sha256"],"live_observation_selection_sha256":observation["legacy_live_observation_selection_sha256"],"receipt_schema":"arc.recovery.legacy-live-observations.v1","labels":["diagnostic","noncanonical","nonreward"],"nodes":live_rows},sort_keys=True,separators=(",",":"))+"\n")
(r/"statuses.jsonl").write_text("".join(json.dumps(x,sort_keys=True,separators=(",",":"))+"\n" for x in rows))
(r/"roots").write_text(f"{freeze_sha} {rollout_sha}\n")
PY
    read -r freeze_sha rollout_sha < "$f/roots"
    chmod 400 "$f/shared/"*
    local shared_input
    for shared_input in "$f/shared/"*; do
        register_shared_input "$shared_input" "$(hash_file "$shared_input")" \
            "$f/catalog" "${shared_input##*/}" || return 1
    done
    archive_sha="$(build_archive_metadata "$f/catalog" "$f/statuses.jsonl" "$f/meta" "$freeze_sha" "$(printf 'b%.0s' {1..64})" "$rollout_sha" "$(printf '1%.0s' {1..40})" "$(hash_file "$f/shared/archive-fleet-to-drive.sh")" "$(hash_file "$f/shared/archive-node.sh")" "$(hash_file "$f/shared/recovery_rollout.py")" "$(hash_file "$f/shared/recovery-manifest.schema.json")" 0 3 3)" || return 1
    local intent_sha
    intent_sha="$(seal_archive_finalization_intent \
        "$f/finalization.json" "$f/catalog" "$f/statuses.jsonl" \
        "$f/meta/SHA256SUMS" "$f/meta/ARCHIVE-MANIFEST.json" \
        "$f/meta/ARCHIVE-MANIFEST.json.sha256" "$freeze_sha" \
        "$(printf 'b%.0s' {1..64})" "$rollout_sha" "$(printf '1%.0s' {1..40})" \
        "local:$remote" FerrumVir)" || return 1
    archive_finalization_intent_roots "$f/finalization.json" "$f/catalog" \
        "$freeze_sha" "$(printf 'b%.0s' {1..64})" "$rollout_sha" \
        "$(printf '1%.0s' {1..40})" "local:$remote" >/dev/null || return 1
    python3 - "$f/finalization.json.gist-anchor.json" "$intent_sha" <<'PY' || return 1
import json,os,pathlib,sys
p=pathlib.Path(sys.argv[1]); h=sys.argv[2]
v={"schema":"arc.recovery.archive-finalization-gist-anchor.v1","provider":"github.com","owner_login":"FerrumVir","visibility":"secret","gist_id":"d"*32,"gist_revision":"e"*40,"gist_filename":"arc-recovery-"+"b"*64+".finalization-intent.json","gist_file_sha256":h,"intent_sha256":h,"created_at":"2026-08-31T00:00:00Z"}
fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o400)
with os.fdopen(fd,"wb") as f:f.write((json.dumps(v,sort_keys=True,separators=(",",":"))+"\n").encode());f.flush();os.fsync(f.fileno())
PY
    build_archive_complete "$f/complete/COMPLETE.json" "$f/finalization.json" \
        "$f/finalization.json.gist-anchor.json" || return 1
    # shellcheck disable=SC2329 # invoked indirectly by the sourced archive helper
    fetch_verify_or_recover_complete_gist_anchor() { :; }
    cp "$f/shared/"* "$remote/"; cp "$f/meta/"* "$remote/"; cp "$f/complete/COMPLETE.json" "$remote/"
    chmod 600 "$remote/"*
    cat > "$f/bin/rclone" <<'SH'
#!/bin/sh
c=$1; shift
case "$c" in
  cat)
    p=${1#local:}
    exec /bin/cat -- "$p"
    ;;
  lsf)
    for a in "$@"; do case "$a" in local:*) r=${a#local:};; esac; done
    find "$r" -maxdepth 1 -type f -print | sed 's!.*/!!'
    ;;
  *) exit 2 ;;
esac
SH
    chmod 700 "$f/bin/rclone"; complete_sha="$(hash_file "$remote/COMPLETE.json")"; sums_sha="$(hash_file "$remote/SHA256SUMS")"
    PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" "" "" "" "$complete_sha" "$archive_sha" "$sums_sha" "$rollout_sha" >/dev/null || return 1
    cp "$remote/legacy-live-observations.json" "$f/legacy-live-observations.json"; printf mutation >> "$remote/legacy-live-observations.json"
    ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    cp "$f/legacy-live-observations.json" "$remote/legacy-live-observations.json"
    printf x >> "$remote/canonical-reference.json"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    cp "$f/shared/canonical-reference.json" "$remote/canonical-reference.json"
    mv "$remote/legacy-nyc.tar.zst" "$f/missing"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    mv "$f/missing" "$remote/legacy-nyc.tar.zst"; printf x >> "$remote/legacy-nyc.tar.zst"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    printf 'nyc-bundle' > "$remote/legacy-nyc.tar.zst"; printf extra > "$remote/EXTRA"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    return 0
)

verify_complete_plan_cleans_transport_state_and_never_uses_ssh() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local fixture digest destination
    fixture="$(mktemp -d "$REPO_ROOT/.verify-complete-plan-test.XXXXXX")"
    trap 'chmod -R u+w "$fixture" 2>/dev/null || true; rm -rf -- "$fixture"' EXIT
    chmod 700 "$fixture"
    mkdir -m 700 "$fixture/success-tmp" "$fixture/failure-tmp"
    digest="$(printf 'a%.0s' {1..64})"
    DRIVE_REMOTE=arc-test
    ARC_RECOVERY_DRIVE_REMOTE=arc-test
    export fixture digest DRIVE_REMOTE ARC_RECOVERY_DRIVE_REMOTE
    destination="$DRIVE_REMOTE/captures/$digest"

    # Exercise the phase boundary rather than Drive/GitHub parsing, which has
    # separate fixtures above.  These mocks materialize the same sensitive
    # pinned roots as configure_operator_transport so cleanup is observable.
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    configure_operator_transport() {
        ARCHIVE_FLEET_PINNED_PYTHON_ROOT="$(mktemp -d)"
        ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT="$(mktemp -d)"
        printf 'operator-python-home\n' > "$ARCHIVE_FLEET_PINNED_PYTHON_ROOT/state"
        printf 'ssh-identity-and-rclone-config\n' > "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/private"
    }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    configure_github_anchor_transport() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    require_commands() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    validate_drive_remote() { return 0; }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    verify_remote_complete() {
        printf '%s\n' "$digest"
    }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    rclone() {
        [ "${ARC_TEST_FAIL_RCLONE:-0}" != 1 ] || die "injected metadata download failure"
        [ "$1" = cat ] || return 74
        case "$2" in
            */freeze-plan.json)
                printf '%s\n' '{"nodes":[{"name":"nyc","data_dir":"/var/lib/arc-old-nyc"},{"name":"lax","data_dir":"/var/lib/arc-old-lax"},{"name":"ams","data_dir":"/var/lib/arc-old-ams"},{"name":"lhr","data_dir":"/var/lib/arc-old-lhr"},{"name":"nrt","data_dir":"/var/lib/arc-old-nrt"},{"name":"sgp","data_dir":"/var/lib/arc-old-sgp"}]}'
                ;;
            */freeze-plan.json.sha256)
                printf '%s  freeze-plan.json\n' "$digest"
                ;;
            *) return 75 ;;
        esac
    }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    freeze_plan_hash() { printf '%s\n' "$digest"; }
    # shellcheck disable=SC2329 # invoked indirectly by verify_complete_phase
    capture_id_for_freeze_plan_hash() { printf '%s\n' "$digest"; }
    # Plan archive verification must not reach either SSH entry point unless
    # execute explicitly requests --verify-live-captures.
    # shellcheck disable=SC2329 # tripwire invoked only on a regression
    ssh() { : > "$fixture/ssh-called"; return 91; }
    # shellcheck disable=SC2329 # tripwire invoked only on a regression
    ssh_remote_exact() { : > "$fixture/ssh-called"; return 92; }

    export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES='configure_operator_transport configure_github_anchor_transport require_commands validate_drive_remote verify_remote_complete rclone freeze_plan_hash capture_id_for_freeze_plan_hash ssh ssh_remote_exact'

    TMPDIR="$fixture/success-tmp" dispatch_archive_command verify_complete_phase \
        --destination "$destination" \
        --new-node-paths nyc /opt/arc-v3-nyc /var/lib/arc-v3-nyc \
        --new-node-paths lax /opt/arc-v3-lax /var/lib/arc-v3-lax \
        --new-node-paths ams /opt/arc-v3-ams /var/lib/arc-v3-ams \
        --new-node-paths lhr /opt/arc-v3-lhr /var/lib/arc-v3-lhr \
        --new-node-paths nrt /opt/arc-v3-nrt /var/lib/arc-v3-nrt \
        --new-node-paths sgp /opt/arc-v3-sgp /var/lib/arc-v3-sgp \
        > "$fixture/success.out" || return 1
    grep -Fq -- "archive_manifest=$digest" "$fixture/success.out" || return 1
    [ ! -e "$fixture/ssh-called" ] || return 1
    [ -z "$(find "$fixture/success-tmp" -mindepth 1 -print -quit)" ] || return 1

    if ARC_TEST_FAIL_RCLONE=1 TMPDIR="$fixture/failure-tmp" \
        dispatch_archive_command verify_complete_phase --destination "$destination" \
        --new-node-paths nyc /opt/arc-v3-nyc /var/lib/arc-v3-nyc \
        --new-node-paths lax /opt/arc-v3-lax /var/lib/arc-v3-lax \
        --new-node-paths ams /opt/arc-v3-ams /var/lib/arc-v3-ams \
        --new-node-paths lhr /opt/arc-v3-lhr /var/lib/arc-v3-lhr \
        --new-node-paths nrt /opt/arc-v3-nrt /var/lib/arc-v3-nrt \
        --new-node-paths sgp /opt/arc-v3-sgp /var/lib/arc-v3-sgp \
        > "$fixture/failure.out" 2>&1; then
        printf 'verify-complete accepted a failed archive verifier\n' >&2
        return 1
    fi
    [ ! -e "$fixture/ssh-called" ] || return 1
    [ -z "$(find "$fixture/failure-tmp" -mindepth 1 -print -quit)" ] || return 1
)

archive_command_scopes_clean_plan_failure_and_nested_success() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local fixture runtime roots_log digest artifact_digest original_identity_sha original_config_sha
    fixture="$(mktemp -d "$REPO_ROOT/.archive-command-scope-test.XXXXXX")"
    trap 'chmod -R u+w "$fixture" 2>/dev/null || true; rm -rf -- "$fixture"' EXIT
    chmod 700 "$fixture"
    runtime="$fixture/runtime"
    roots_log="$fixture/roots.tsv"
    mkdir -m 700 "$runtime"
    : > "$roots_log"
    printf 'sealed-test-identity\n' > "$fixture/id_ed25519"
    printf 'sealed-test-oauth-config\n' > "$fixture/rclone.conf"
    chmod 400 "$fixture/id_ed25519"
    chmod 600 "$fixture/rclone.conf"
    original_identity_sha="$(hash_file "$fixture/id_ed25519")"
    original_config_sha="$(hash_file "$fixture/rclone.conf")"
    digest="$(printf '9%.0s' {1..64})"
    printf '{}\n' > "$fixture/legacy-validators.json"
    printf 'capture-plan-input\n' > "$fixture/capture-input"
    artifact_digest="$(hash_file "$fixture/capture-input")"
    export fixture roots_log digest artifact_digest
    for name in freeze.json height.json inspector genesis.toml validators.json capture-legacy.json; do
        cp "$fixture/capture-input" "$fixture/$name"
    done

    mode_of() {
        python3 - "$1" <<'PY'
import pathlib, stat, sys
print(f"{stat.S_IMODE(pathlib.Path(sys.argv[1]).stat().st_mode):03o}")
PY
    }
    assert_logged_roots_absent() {
        local kind path drive
        while IFS=$'\t' read -r kind path drive; do
            [ -n "$kind" ] && [ -n "$path" ] || return 1
            [ ! -e "$path" ] && [ ! -L "$path" ] || {
                printf 'temporary %s root (Drive=%s) survived command exit: %s\n' \
                    "$kind" "$drive" "$path" >&2
                return 1
            }
        done < "$roots_log"
    }
    assert_original_credentials_unchanged() {
        [ "$(hash_file "$fixture/id_ed25519")" = "$original_identity_sha" ] &&
            [ "$(hash_file "$fixture/rclone.conf")" = "$original_config_sha" ] &&
            [ "$(mode_of "$fixture/id_ed25519")" = 400 ] &&
            [ "$(mode_of "$fixture/rclone.conf")" = 600 ]
    }

    # These test doubles materialize the same three private classes as the
    # real configuration helpers: nonsecret Python HOME, SSH identity, and
    # OAuth-bearing rclone config. The command boundary, not parsing/network
    # behavior (covered elsewhere), is under test here.
    # shellcheck disable=SC2329 # invoked through each command function
    configure_operator_python() {
        ARCHIVE_FLEET_PINNED_PYTHON_ROOT="$(mktemp -d "$TMPDIR/python-home.XXXXXX")"
        chmod 700 "$ARCHIVE_FLEET_PINNED_PYTHON_ROOT"
        printf 'isolated-python-home\n' > "$ARCHIVE_FLEET_PINNED_PYTHON_ROOT/state"
        [ "$(mode_of "$ARCHIVE_FLEET_PINNED_PYTHON_ROOT")" = 700 ] || return 1
        printf 'python\t%s\tfalse\n' "$ARCHIVE_FLEET_PINNED_PYTHON_ROOT" >> "$roots_log"
    }
    # shellcheck disable=SC2329 # invoked through each command function
    configure_operator_transport() {
        local require_drive="${1:-false}"
        configure_operator_python
        ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT="$(mktemp -d "$TMPDIR/transport.XXXXXX")"
        chmod 700 "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT"
        cp "$fixture/id_ed25519" "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/id_ed25519"
        chmod 400 "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/id_ed25519"
        if [ "$require_drive" = true ]; then
            cp "$fixture/rclone.conf" "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/rclone.conf"
            chmod 600 "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/rclone.conf"
            printf 'simulated-token-refresh\n' >> "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/rclone.conf"
            [ "$(mode_of "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/rclone.conf")" = 600 ] || return 1
        fi
        [ "$(mode_of "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT")" = 700 ] || return 1
        [ "$(mode_of "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT/id_ed25519")" = 400 ] || return 1
        printf 'transport\t%s\t%s\n' "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT" "$require_drive" >> "$roots_log"
        [ "${ARC_TEST_CONFIGURE_FAIL:-0}" != 1 ] || die "injected transport configuration failure"
    }
    # shellcheck disable=SC2329 # invoked indirectly by command functions
    require_commands() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by prepare-writers
    tracked_source_hash() { printf '%s\n' "$digest"; }
    # shellcheck disable=SC2329 # invoked indirectly by prepare-writers
    install_helpers() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by prepare-writers
    run_remote() { :; }

    export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES='mode_of configure_operator_python configure_operator_transport require_commands tracked_source_hash install_helpers run_remote'

    TMPDIR="$runtime" dispatch_archive_command prepare_writers \
        --legacy-validator-set "$fixture/legacy-validators.json" \
        --output "$fixture/writers-plan.json" --plan > "$fixture/plan.out" || return 1
    grep -Fq -- 'archive fleet: PLAN ONLY;' "$fixture/plan.out" || return 1
    assert_logged_roots_absent || return 1
    assert_original_credentials_unchanged || return 1

    # A real capture plan asks for Drive transport, so this success path
    # exercises simultaneous Python-HOME, SSH-identity, and OAuth-config copies.
    # Its heavyweight evidence validators are independently covered above.
    # shellcheck disable=SC2329 # invoked indirectly by capture plan
    pin_freeze_plan() { printf '%s\n' "$1"; }
    # shellcheck disable=SC2329 # invoked indirectly by capture plan
    freeze_plan_hash() { printf '%s\n' "$digest"; }
    # shellcheck disable=SC2329 # invoked indirectly by capture plan
    capture_id_for_freeze_plan_hash() { printf '%s\n' "$digest"; }
    # shellcheck disable=SC2329 # invoked indirectly by capture plan
    manifest_field() { printf '%s\n' "$digest"; }
    # shellcheck disable=SC2329 # invoked indirectly by capture plan
    run_drive_prefreeze_gate() { :; }
    ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES="$ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES pin_freeze_plan freeze_plan_hash capture_id_for_freeze_plan_hash manifest_field run_drive_prefreeze_gate"
    export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES
    TMPDIR="$runtime" dispatch_archive_command capture_phase \
        --freeze-plan "$fixture/freeze.json" \
        --legacy-public-height-receipt "$fixture/height.json" \
        --legacy-public-height-receipt-sha256 "$artifact_digest" \
        --inspector-binary "$fixture/inspector" \
        --inspector-binary-sha256 "$artifact_digest" \
        --genesis "$fixture/genesis.toml" \
        --genesis-sha256 "$artifact_digest" \
        --validator-public-keys "$fixture/validators.json" \
        --validator-public-keys-sha256 "$artifact_digest" \
        --legacy-validator-set "$fixture/capture-legacy.json" \
        --legacy-validator-set-sha256 "$artifact_digest" \
        --offline-stop-evidence-output "$fixture/offline-stop.json" \
        --plan > "$fixture/capture-plan.out" || return 1
    grep -Fq -- \
        'PLAN ONLY; no persistent service or recovery-managed remote/local file was changed' \
        "$fixture/capture-plan.out" || return 1
    assert_logged_roots_absent || return 1
    assert_original_credentials_unchanged || return 1

    # Help is an early successful return. It must retain its output/status and
    # allocate no runtime state after begin_temporary_scope installs cleanup.
    local roots_before_help
    roots_before_help="$(wc -l < "$roots_log" | tr -d ' ')"
    TMPDIR="$runtime" dispatch_archive_command prepare_writers --help > "$fixture/help.out" || return 1
    grep -Fq -- 'Usage:' "$fixture/help.out" || return 1
    [ "$(wc -l < "$roots_log" | tr -d ' ')" = "$roots_before_help" ] || return 1

    # Exercise the successful prepare -> nested audit shape. The nested scope
    # first forgets inherited ownership, then proves its own cleanup removed
    # only its roots while all three parent roots still exist.
    # shellcheck disable=SC2329 # invoked indirectly by prepare-writers
    audit_writers() {
        local parent_temp="$ARCHIVE_FLEET_TEMP_ROOT"
        local parent_transport="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT"
        local parent_python="$ARCHIVE_FLEET_PINNED_PYTHON_ROOT"
        begin_temporary_scope
        configure_operator_transport false
        local nested_transport="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT"
        local nested_python="$ARCHIVE_FLEET_PINNED_PYTHON_ROOT"
        nested_cleanup_check() {
            cleanup_temporary_root
            if [ -d "$parent_temp" ] && [ -d "$parent_transport" ] && [ -d "$parent_python" ] &&
                [ ! -e "$nested_transport" ] && [ ! -e "$nested_python" ]; then
                printf 'PASS\n' > "$fixture/nested-scope.out"
            else
                printf 'FAIL\n' > "$fixture/nested-scope.out"
                return 1
            fi
        }
        trap nested_cleanup_check EXIT
        nested_cleanup_check
        trap - EXIT
    }
    ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES="$ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES audit_writers"
    export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES
    ARC_RECOVERY_PREPARE_GO="STAGE-BARRIERS $digest HELPER $digest" \
        TMPDIR="$runtime" dispatch_archive_command prepare_writers \
        --legacy-validator-set "$fixture/legacy-validators.json" \
        --output "$fixture/writers-execute.json" --execute > "$fixture/execute.out" || return 1
    [ "$(cat "$fixture/nested-scope.out")" = PASS ] || return 1
    assert_logged_roots_absent || return 1
    [ -z "$(find "$runtime" -mindepth 1 -print -quit)" ] || return 1

    # Fail inside transport configuration, after all three private classes
    # exist but before the helper returns. The command's already-installed
    # EXIT handler must still remove every copy, including the OAuth config.
    # shellcheck disable=SC2329 # invoked indirectly by verify-complete
    validate_drive_remote() { return 0; }
    ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES="$ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES validate_drive_remote"
    export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES
    if ARC_TEST_CONFIGURE_FAIL=1 TMPDIR="$runtime" \
        dispatch_archive_command verify_complete_phase --destination "arc-test:captures/$digest" \
        > "$fixture/configure-failure.out" 2>&1; then
        printf 'verify-complete accepted injected transport setup failure\n' >&2
        return 1
    fi
    assert_logged_roots_absent || return 1
    assert_original_credentials_unchanged || return 1

    # The local freeze-plan command configures only its private Python HOME.
    # A validation error after configuration must clean that nonsecret root.
    if TMPDIR="$runtime" dispatch_archive_command seal_freeze_plan \
        > "$fixture/python-failure.out" 2>&1; then
        printf 'seal-freeze-plan accepted empty required arguments\n' >&2
        return 1
    fi
    assert_logged_roots_absent || return 1
    assert_original_credentials_unchanged || return 1
    [ -z "$(find "$runtime" -mindepth 1 -print -quit)" ] || return 1
)

archive_dispatcher_preserves_errexit_and_accepts_completed_takeover() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local fixture runtime startup_gate startup_supervisor startup_phase
    local startup_sentinel startup_sentinel_pgid startup_token attempt
    fixture="$(mktemp -d "$REPO_ROOT/.archive-dispatch-errexit-test.XXXXXX")"
    trap 'chmod -R u+w "$fixture" 2>/dev/null || true; rm -rf -- "$fixture"' EXIT
    chmod 700 "$fixture"
    runtime="$fixture/runtime"
    mkdir -m 700 "$runtime"
    export ARC_TEST_FAILFAST_MARKER="$fixture/mutation-after-failure"

    # A command function must be invoked as a direct simple command. Calling it
    # from an if/!/&&/|| condition disables errexit throughout the function and
    # would let this marker mutation run after the failed prerequisite.
    capture_phase() {
        begin_temporary_scope
        false
        : > "$ARC_TEST_FAILFAST_MARKER"
    }
    export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES=capture_phase
    if TMPDIR="$runtime" dispatch_archive_command capture_phase \
        > "$fixture/failfast.out" 2>&1; then
        printf 'dispatcher accepted an early phase failure\n' >&2
        return 1
    fi
    [ ! -e "$fixture/mutation-after-failure" ] || {
        printf 'dispatcher disabled phase errexit and continued into mutation\n' >&2
        return 1
    }
    [ -z "$(find "$runtime" -mindepth 1 -print -quit)" ] || {
        printf 'errexit phase left runtime residue: %s\n' \
            "$(find "$runtime" -mindepth 1 | head -5 | tr '\n' ' ')" >&2
        return 1
    }

    # The guardian takeover/acknowledge handshake this block exercised belonged
    # to dispatch_archive_command_legacy_unused, which was unreachable and has
    # been removed. The live design has no in-gate ack protocol: the sentinel
    # sweeps on the guardian completion receipt alone. Covered now by the
    # static contract ordering assertions and by the signal matrix below.

    # Kill both a phase that has published readiness and its supervisor before
    # any watchdog exists. The self-publishing sentinel must notice its own
    # PPID change, remove the pre-GO gate, and exit; numeric supervisor liveness
    # must play no role.
    startup_gate="$(mktemp -d "$runtime/arc-archive-dispatch.XXXXXX")"
    chmod 700 "$startup_gate"
    mkdir -m 700 "$startup_gate/runtime"
    /bin/bash -c '
set -Eeuo pipefail
# Read the positionals BEFORE clearing them: the previous order ran "set --"
# first, so "$1"/"$2" were already gone and this whole block died on the
# empty source argument instead of exercising supervisor death.
orchestrator=$1
gate=$2
set --
. "$orchestrator" >/dev/null
archive_write_current_process_id "$gate/test-supervisor.pid"
IFS= read -r supervisor_pid < "$gate/test-supervisor.pid"
set -m
( archive_dispatch_phase "$supervisor_pid" "$gate" capture_phase ) &
printf "%s\t%s\n" "$supervisor_pid" "$!" > "$gate/test-startup.info"
while :; do /bin/sleep 1; done
' arc-archive-startup-test "$ORCHESTRATOR" "$startup_gate" &
    startup_supervisor="$!"
    # Ceiling, not a deadline. 500*0.02s = 10s proved too tight on a loaded
    # Linux CI runner and flaked this test once on PR #81 while the assertions
    # themselves were correct. Nothing here weakens: the checks below still
    # require readiness to have been published.
    for ((attempt = 0; attempt < 1500; attempt += 1)); do
        [ -f "$startup_gate/phase.ready" ] && [ -f "$startup_gate/test-startup.info" ] && break
        /bin/sleep 0.02
    done
    [ -f "$startup_gate/phase.ready" ] && [ -f "$startup_gate/test-startup.info" ] || {
        printf 'startup phase never published readiness: gate=[%s]\n' \
            "$(ls -A "$startup_gate" 2>/dev/null | tr '\n' ' ')" >&2
        return 1
    }
    IFS=$'\t' read -r recorded_supervisor startup_phase < "$startup_gate/test-startup.info"
    [ "$recorded_supervisor" = "$startup_supervisor" ] || {
        printf 'recorded supervisor %s != spawned %s\n' \
            "$recorded_supervisor" "$startup_supervisor" >&2
        return 1
    }
    IFS=$'\t' read -r startup_sentinel startup_sentinel_pgid startup_token \
        < "$startup_gate/sentinel.ready"
    # The previous check was "*[!0-9:ARC-HIVE_STOP-]*", where C-H is a bracket
    # RANGE (C,D,E,F,G,H), so tokens like POTATO:12:34 passed. Assert the real
    # shape: two numeric ids plus the ARC-ARCHIVE-STOP: prefixed token.
    case "$startup_sentinel" in ""|*[!0-9]*)
        printf 'sentinel.ready pid not numeric: [%s]\n' "$startup_sentinel" >&2
        return 1 ;;
    esac
    case "$startup_sentinel_pgid" in ""|*[!0-9]*)
        printf 'sentinel.ready pgid not numeric: [%s]\n' "$startup_sentinel_pgid" >&2
        return 1 ;;
    esac
    case "$startup_token" in ARC-ARCHIVE-STOP:*) ;; *)
        printf 'sentinel.ready token lacks prefix: [%s]\n' "$startup_token" >&2
        return 1 ;;
    esac
    builtin kill -s KILL -- "$startup_phase" "$startup_supervisor" 2>/dev/null || true
    wait "$startup_supervisor" 2>/dev/null || true
    # Ceiling, not a deadline. The pre-watchdog sentinel must notice its own
    # PPID change after the double SIGKILL and sweep the gate; 10s is tight on
    # a contended runner. The assertion immediately below is unchanged and
    # still fails if the gate survives.
    for ((attempt = 0; attempt < 1500; attempt += 1)); do
        { [ ! -e "$startup_gate" ] && [ ! -L "$startup_gate" ]; } && break
        /bin/sleep 0.02
    done
    [ ! -e "$startup_gate" ] && [ ! -L "$startup_gate" ] || {
        printf 'pre-watchdog sentinel did not self-clean after phase/supervisor loss\n' >&2
        return 1
    }
    # As the last command of this () subshell, "cmd && return 1" fails BOTH
    # ways: present -> return 1; absent -> the compound takes the helper's own
    # non-zero status. Negate so absence is the success case.
    # The sentinel removes the gate and THEN exits, so gate absence does not
    # imply the process is already reaped. Asserting instantly made this test
    # fail under full-harness load (observed: gate swept, sentinel still alive a
    # moment later) while passing 5/5 standalone. Wait for the exit on a bounded
    # ceiling; the assertion below is unchanged and still fails if it never goes.
    for ((attempt = 0; attempt < 1500; attempt += 1)); do
        archive_process_exists "$startup_sentinel" || break
        /bin/sleep 0.02
    done
    ! archive_process_exists "$startup_sentinel" || {
        printf 'pre-watchdog sentinel %s survived phase+supervisor SIGKILL\n' \
            "$startup_sentinel" >&2
        return 1
    }
)

archive_dispatcher_signals_stop_the_full_phase_group_and_clean() (
    local fixture python_bin
    fixture="$(mktemp -d "$REPO_ROOT/.archive-dispatch-signal-test.XXXXXX")"
    trap 'chmod -R u+w "$fixture" 2>/dev/null || true; rm -rf -- "$fixture"' EXIT
    chmod 700 "$fixture"
    python_bin="$(type -P python3)" || return 1
    [ -x "$python_bin" ] || return 1

    python3 - "$ORCHESTRATOR" "$SIGNAL_PROBE" "$fixture" "$python_bin" <<'PY' || return 1
import hashlib
import os
import pathlib
import signal
import stat
import subprocess
import sys
import time

orchestrator = pathlib.Path(sys.argv[1])
probe = pathlib.Path(sys.argv[2])
fixture = pathlib.Path(sys.argv[3])
python_bin = pathlib.Path(sys.argv[4])
identity = fixture / "id_ed25519"
config = fixture / "rclone.conf"
identity.write_bytes(b"sealed-test-identity\n")
config.write_bytes(b"sealed-test-oauth-config\n")
identity.chmod(0o400)
config.chmod(0o600)

def snapshot(path):
    details = path.lstat()
    return hashlib.sha256(path.read_bytes()).hexdigest(), stat.S_IMODE(details.st_mode), details.st_ino

sealed_identity = snapshot(identity)
sealed_config = snapshot(config)

def process_rows():
    result = subprocess.run(
        ["/bin/ps", "-ax", "-o", "pid=", "-o", "pgid=", "-o", "stat="],
        check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    rows = []
    for raw in result.stdout.splitlines():
        fields = raw.split(None, 2)
        if len(fields) == 3:
            rows.append((int(fields[0]), int(fields[1]), fields[2]))
    return rows

def live_pid(pid):
    return any(row_pid == pid and not state.startswith("Z")
               for row_pid, _pgid, state in process_rows())

def live_group(pgid):
    return any(row_pgid == pgid and not state.startswith("Z")
               for _pid, row_pgid, state in process_rows())

def wait_until(predicate, seconds, label):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {label}")

def read_info(path):
    raw = path.read_text(encoding="utf-8").strip().split("\t")
    if len(raw) != 2 or not all(item.isdigit() for item in raw):
        raise AssertionError(f"malformed process proof: {path}")
    return int(raw[0]), int(raw[1])

def read_sentinel_info(path):
    raw = path.read_text(encoding="utf-8").strip().split("\t")
    if len(raw) != 3 or not raw[0].isdigit() or not raw[1].isdigit() or not raw[2]:
        raise AssertionError(f"malformed sentinel proof: {path}")
    return int(raw[0]), int(raw[1]), raw[2]

command = [
    "/bin/bash", "-c",
    'archive_source=$1; probe_source=$2; set --; '
    '. "$archive_source" >/dev/null; . "$probe_source"; '
    # The phase is a fresh interpreter that unsets every inherited function and
    # re-sources only the names listed here. The sweep runs in the sentinel,
    # inside the phase -- so a gate-removal double declared only in this
    # supervisor shell never reached the code under test and the injection
    # silently never fired. Name it too, but only when the probe defines it:
    # the snapshot writer rejects a name with no function behind it.
    'overrides=capture_phase; '
    '[ "${ARC_SIGNAL_GATE_REMOVE_FAIL_ONCE:-false}" = true ] && '
    'overrides="capture_phase archive_remove_dispatch_gate"; '
    '[ "${ARC_SIGNAL_SENTINEL_MOVE_FAIL_ONCE:-false}" = true ] && '
    'overrides="$overrides archive_sentinel_atomic_move"; '
    'export ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES="$overrides"; '
    'dispatch_archive_command capture_phase',
    "archive-dispatch-signal-test", str(orchestrator), str(probe),
]
signals = (("HUP", signal.SIGHUP, 129), ("INT", signal.SIGINT, 130),
           ("TERM", signal.SIGTERM, 143))
cases = [
    (f"{label}_STRESS_{index:02d}", sent, status, False, False, 0, False, False)
    for index in range(50)
    for label, sent, status in (signals[index % len(signals)],)
]
cases.extend((
    ("HUP_IGNORING_CHILD", signal.SIGHUP, 129, True, False, 0, False, False),
    ("INT_IGNORING_CHILD", signal.SIGINT, 130, True, False, 0, False, False),
    ("TERM_IGNORING_CHILD", signal.SIGTERM, 143, True, False, 0, False, False),
    ("TERM_RESURRECTING_CHILD", signal.SIGTERM, 143, False, True, 0, False, False),
    ("TERM_SLOW_EXIT_CLEANUP", signal.SIGTERM, 143, False, False, 10, False, False),
    ("TERM_GATE_SWEEP_RETRY", signal.SIGTERM, 143, False, False, 0, True, False),
    ("TERM_SENTINEL_MOVE_RETRY", signal.SIGTERM, 143, False, False, 0, False, True),
    ("KILL", signal.SIGKILL, 137, False, False, 0, False, False),
))

fast_teardowns = []
for (name, sent_signal, expected_status, child_ignores, child_resurrects,
     cleanup_delay, gate_remove_fails, sentinel_move_fails) in cases:
    case = fixture / name.lower()
    runtime = case / "runtime"
    case.mkdir(mode=0o700)
    runtime.mkdir(mode=0o700)
    environment = os.environ.copy()
    environment.update({
        "TMPDIR": str(runtime),
        "ARC_SIGNAL_CASE_DIR": str(case),
        "ARC_SIGNAL_PYTHON": str(python_bin),
        "ARC_SIGNAL_IDENTITY": str(identity),
        "ARC_SIGNAL_RCLONE_CONFIG": str(config),
        "ARC_SIGNAL_FOREGROUND_IGNORES": "true" if child_ignores else "false",
        "ARC_SIGNAL_BACKGROUND_RESURRECTS": "true" if child_resurrects else "false",
        "ARC_SIGNAL_CLEANUP_DELAY_SECONDS": str(cleanup_delay),
        "ARC_SIGNAL_GATE_REMOVE_FAIL_ONCE": "true" if gate_remove_fails else "false",
        "ARC_SIGNAL_SENTINEL_MOVE_FAIL_ONCE": "true" if sentinel_move_fails else "false",
    })
    stdout_path = case / "supervisor.stdout"
    stderr_path = case / "supervisor.stderr"
    phase_pid = phase_pgid = sentinel_pid = background_pid = foreground_pid = watchdog_pid = None
    process = None
    try:
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            process = subprocess.Popen(
                command, env=environment, stdout=stdout, stderr=stderr,
                start_new_session=True,
            )
            ready = (case / "phase.info", case / "background.info", case / "foreground.info")
            wait_until(lambda: all(path.is_file() for path in ready), 10, f"{name} phase readiness")
            phase_pid, phase_pgid = read_info(case / "phase.info")
            background_pid, background_pgid = read_info(case / "background.info")
            foreground_pid, foreground_pgid = read_info(case / "foreground.info")
            gates = list(runtime.glob("arc-archive-dispatch.*"))
            if len(gates) != 1:
                raise AssertionError(f"{name} dispatcher gate count differs: {gates}")
            wait_until(lambda: (gates[0] / "watchdog.ready").is_file(), 5,
                       f"{name} watchdog readiness")
            sentinel_pid, sentinel_pgid, sentinel_token = read_sentinel_info(
                gates[0] / "sentinel.ready"
            )
            # Production writes watchdog.ready as pid \t pgid \t stop_token
            # (three fields). read_info demands exactly two, so every case in
            # this matrix died here on "malformed process proof" before a
            # single signal was ever delivered.
            watchdog_pid, watchdog_pgid, watchdog_token = read_sentinel_info(
                gates[0] / "watchdog.ready"
            )
            if watchdog_token != sentinel_token:
                raise AssertionError(
                    f"{name} watchdog and sentinel disagree on the stop token"
                )
            if not (phase_pid == phase_pgid == sentinel_pgid == background_pgid == foreground_pgid):
                raise AssertionError(f"{name} phase descendants escaped the dedicated group")
            if sentinel_pid in {phase_pid, process.pid} or not sentinel_token.startswith(
                    "ARC-ARCHIVE-STOP:"):
                raise AssertionError(f"{name} sentinel identity differs")
            if process.pid == phase_pid or watchdog_pgid in {phase_pgid, process.pid}:
                raise AssertionError(f"{name} supervisor/watchdog group separation differs")
            for pid in (process.pid, phase_pid, sentinel_pid, background_pid,
                        foreground_pid, watchdog_pid):
                if not live_pid(pid):
                    raise AssertionError(f"{name} process was not live before the signal: {pid}")

            roots = []
            for line in (case / "roots.tsv").read_text(encoding="utf-8").splitlines():
                kind, raw_path = line.split("\t")
                path = pathlib.Path(raw_path)
                roots.append((kind, path))
                details = path.lstat()
                if path.is_symlink() or not stat.S_ISDIR(details.st_mode) or stat.S_IMODE(details.st_mode) != 0o700:
                    raise AssertionError(f"{name} {kind} root is not a private directory")
                try:
                    path.relative_to(gates[0] / "runtime")
                except ValueError as error:
                    raise AssertionError(f"{name} {kind} root escaped the dispatcher gate") from error
            if [kind for kind, _path in roots] != ["scratch", "pinned", "transport", "python"]:
                raise AssertionError(f"{name} did not register all four temp-root classes")
            transport = dict(roots)["transport"]
            if stat.S_IMODE((transport / "id_ed25519").lstat().st_mode) != 0o400:
                raise AssertionError(f"{name} temporary SSH identity mode differs")
            if stat.S_IMODE((transport / "rclone.conf").lstat().st_mode) != 0o600:
                raise AssertionError(f"{name} temporary OAuth config mode differs")
            if (transport / "rclone.conf").read_bytes() == config.read_bytes():
                raise AssertionError(f"{name} temporary OAuth refresh was not exercised")

            # Signal only the dispatcher parent. Its trap/guardian must close
            # the entire phase group; the test never signals a child directly.
            os.kill(process.pid, sent_signal)
            # Budget per case profile, measured on bash 3.2 / macOS after the
            # gate-sweep fix: baseline teardown is ~2.2s, a 10s injected
            # cleanup delay lands at ~12.2s, and a TERM-ignoring foreground
            # child forces the guardian's full escalation ladder (bounded TERM
            # wait, STOP + exact-PID KILL, drain, gate-absence poll) at ~43.7s.
            # A single flat 60s would pass all of these while hiding a
            # regression in the 50 fast stress cases, so keep those tight.
            # Ceiling only. A tight per-case timeout flakes: HUP_STRESS_00
            # measures ~2.2s alone but exceeded 30s inside a full run, purely
            # from the preceding suites' teardown contending for CPU. Teardown
            # SPEED is asserted after the loop on the median of the fast cases,
            # which one contended outlier cannot flake but which still catches
            # a regression pushing ordinary signals into the ~43s escalation
            # ladder.
            wait_budget = 120 + cleanup_delay
            signal_sent_at = time.monotonic()
            try:
                raw_status = process.wait(timeout=wait_budget)
            except subprocess.TimeoutExpired as error:
                # Name the case AND say where it is stuck. A bare
                # TimeoutExpired traceback gives no clue which of the 58 cases
                # hung, nor why, which is most of the debugging.
                diagnosis = [f"{name} did not exit within {wait_budget}s of {sent_signal}"]
                try:
                    diagnosis.append(f"stderr={stderr_path.read_text(encoding='utf-8')[-1500:]!r}")
                except OSError as exc:
                    diagnosis.append(f"stderr unreadable: {exc}")
                try:
                    diagnosis.append(f"gate={sorted(entry.name for entry in gates[0].iterdir())}")
                except OSError as exc:
                    diagnosis.append(f"gate unreadable: {exc}")
                diagnosis.append(
                    f"group_rows={[row for row in process_rows() if row[1] == phase_pgid]}"
                )
                diagnosis.append(
                    "arc_env=" + repr(sorted(
                        key for key in environment if key.startswith("ARC_")
                    ))
                )
                # If the shell that launched this python had HUP ignored, the
                # disposition is inherited and a non-interactive child bash
                # CANNOT trap it (POSIX): the signal is silently discarded and
                # the dispatcher waits forever. That is exactly this symptom.
                diagnosis.append(
                    "inherited_dispositions=" + repr({
                        name: str(signal.getsignal(getattr(signal, name)))
                        for name in ("SIGHUP", "SIGINT", "SIGTERM")
                    })
                )
                try:
                    cwd = os.getcwd()
                    diagnosis.append(f"cwd={cwd!r} exists={os.path.isdir(cwd)}")
                except OSError as exc:
                    diagnosis.append(f"cwd UNAVAILABLE: {exc}")
                raise AssertionError(" | ".join(diagnosis)) from error
            if not child_ignores and not cleanup_delay:
                fast_teardowns.append(time.monotonic() - signal_sent_at)
            status = 128 - raw_status if raw_status < 0 else raw_status
            if status != expected_status:
                raise AssertionError(f"{name} dispatcher status {status}, expected {expected_status}")
            cleanup = case / "cleanup.complete"
            if name != "KILL" and not gate_remove_fails and any(
                    path.exists() or path.is_symlink() for _kind, path in roots):
                raise AssertionError(f"{name} supervisor returned before its final root sweep")
            if name != "KILL" and not gate_remove_fails and any(runtime.iterdir()):
                raise AssertionError(f"{name} supervisor returned before removing its private gate")
            if gate_remove_fails:
                if not (case / "gate-remove-failed-once").is_dir():
                    raise AssertionError(f"{name} did not inject the final-sweep failure")
                # The live retry path emits this exact line from
                # archive_remove_dispatch_gate_until_absent. The previously
                # asserted "FATAL could not remove private dispatch gate"
                # appears nowhere in the tree.
                if "FATAL guardian retaining and retrying private dispatch gate" not in \
                        stderr_path.read_text(encoding="utf-8"):
                    raise AssertionError(f"{name} cleanup handoff failure was not loud")
            if sentinel_move_fails and not (case / "sentinel-move-failed-once").is_file():
                raise AssertionError(f"{name} did not inject the sentinel move failure")
            if (not child_ignores and not child_resurrects and not cleanup_delay
                    and name != "KILL" and not cleanup.is_file()):
                time.sleep(0.2)
                failed_cleanup = case / "cleanup.failed"
                details = failed_cleanup.read_text(encoding="utf-8") if failed_cleanup.is_file() else "absent"
                raise AssertionError(
                    f"{name} supervisor returned before phase EXIT cleanup "
                    f"(raw status {raw_status}; cleanup after 200ms={cleanup.is_file()}; "
                    f"failure={details!r}; case entries={sorted(path.name for path in case.iterdir())})"
                )
            if child_resurrects:
                if not (case / "resurrection.attempted").is_file():
                    raise AssertionError(f"{name} did not exercise post-cleanup root resurrection")
            elif cleanup_delay:
                if not (case / "cleanup.started").is_file():
                    raise AssertionError(f"{name} did not enter its deliberately slow EXIT cleanup")
            elif child_ignores:
                # A signal-ignoring foreground child forces the guardian's
                # escalation ladder, so the phase's EXIT cleanup is driven by
                # whichever signal actually reaches it. Bash 3.2 additionally
                # consumes INT while waiting on such a child, so the phase's own
                # INT trap never runs. Measured on bash 3.2 / macOS: HUP -> 129
                # (own trap wins), INT -> 143 (guardian TERM), TERM -> 143; a
                # further escalation to KILL leaves no marker at all (137).
                # The contracts that matter are asserted separately and
                # unconditionally above: the exact caller-visible status, an
                # emptied gate, and no surviving credential root.
                if cleanup.is_file() and cleanup.read_text(encoding="utf-8") not in {
                        f"cleanup-complete exit={expected_status}\n",
                        "cleanup-complete exit=143\n",
                        "cleanup-complete exit=137\n",
                }:
                    raise AssertionError(
                        f"{name} cleanup status differs: "
                        f"{cleanup.read_text(encoding='utf-8')!r}"
                    )
            else:
                wait_until(cleanup.is_file, 15, f"{name} phase cleanup")
                expected_cleanup_status = 143 if name == "KILL" else expected_status
                if cleanup.read_text(encoding="utf-8") != f"cleanup-complete exit={expected_cleanup_status}\n":
                    raise AssertionError(f"{name} cleanup status differs")
            wait_until(lambda: not any(live_pid(pid) for pid in (
                phase_pid, sentinel_pid, background_pid, foreground_pid, watchdog_pid,
            )), 15, f"{name} processes to exit")
            wait_until(lambda: not live_group(phase_pgid), 15, f"{name} phase group to exit")
            wait_until(lambda: not any(runtime.iterdir()), 15, f"{name} runtime cleanup")
            if any(path.exists() or path.is_symlink() for _kind, path in roots):
                raise AssertionError(f"{name} left a temporary root behind")
            if not child_resurrects and not cleanup_delay and (case / "cleanup.failed").exists():
                raise AssertionError(f"{name} phase cleanup reported failure")
            if snapshot(identity) != sealed_identity or snapshot(config) != sealed_config:
                raise AssertionError(f"{name} changed an original credential/config")
    finally:
        if process is not None and process.poll() is None:
            try: os.kill(process.pid, signal.SIGKILL)
            except ProcessLookupError: pass
            try: process.wait(timeout=2)
            except subprocess.TimeoutExpired: pass
        if phase_pgid is not None and live_group(phase_pgid):
            try: os.killpg(phase_pgid, signal.SIGKILL)
            except ProcessLookupError: pass

if fast_teardowns:
    ordered = sorted(fast_teardowns)
    median_teardown = ordered[len(ordered) // 2]
    # Measured ~2.2s on bash 3.2 / macOS. The guardian escalation ladder
    # measures ~43s, so a median above 15s means ordinary signal teardown
    # started escalating -- the regression a per-case timeout used to catch.
    if median_teardown > 15:
        raise AssertionError(
            f"median fast-path teardown regressed to {median_teardown:.1f}s "
            f"across {len(ordered)} cases (expected ~2s)"
        )
PY
)

complete_is_last_and_fully_verified() {
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
t=pathlib.Path(sys.argv[1]).read_text(); t=t[t.index("seal_phase()"):]
m=['stream_bundle_to_drive "$node"','build_archive_metadata \\','rclone copy "$metadata_root"','rclone check "$metadata_root"','upload_immutable "$complete_root/COMPLETE.json"','verify_remote_complete "$destination"']; p=[t.index(x) for x in m[:-1]]+[t.rindex(m[-1])]; assert p==sorted(p); assert 'existing COMPLETE.json fully verified; verification-only resume' in t
assert t.index('run_github_gist_anchor_canary') < t.index('install_helpers')
assert 'arc.recovery.archive-complete.v2' in pathlib.Path(sys.argv[1]).read_text()
assert '"/gists/$gist_id/$gist_revision"' in pathlib.Path(sys.argv[1]).read_text()
assert 'fetch_verify_or_recover_complete_gist_anchor' in t
PY
}

gist_revision_recovers_lost_local_intent_after_latest_edit() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local f; f="$(realpath "$(mktemp -d)")"; trap 'rm -rf -- "$f"' EXIT
    mkdir -m 700 "$f/protected"
    python3 - "$f" <<'PY' || return 1
import hashlib,json,pathlib,sys
r=pathlib.Path(sys.argv[1]);h=lambda b:hashlib.sha256(b).hexdigest();c=lambda v:(json.dumps(v,sort_keys=True,separators=(",",":"))+"\n").encode()
freeze="a"*64;capture="b"*64;rollout="c"*64;manifest="d"*64;gid="e"*32;revision="f"*40
intent={"schema":"arc.recovery.archive-finalization-intent.v2","source_commit":"1"*40,"freeze_plan_sha256":freeze,"capture_id":capture,"prearchive_rollout_sha256":rollout,"destination":"local:archive","destination_sha256":h(b"local:archive"),"archive_manifest":{"name":"ARCHIVE-MANIFEST.json","size":1,"sha256":manifest},"archive_manifest_sidecar":{"name":"ARCHIVE-MANIFEST.json.sha256","size":1,"sha256":"2"*64},"sha256sums":{"name":"SHA256SUMS","size":1,"sha256":"3"*64},"shared_inputs":[],"validator_bundles":[],"capture_classification_counts":{},"github_anchor_policy":{"provider":"github.com","owner_login":"FerrumVir","visibility":"secret","filename":f"arc-recovery-{capture}.finalization-intent.json"}}
raw=c(intent);intent_sha=h(raw);(r/"expected-intent.json").write_bytes(raw)
anchor={"intent_sha256":intent_sha,"gist_id":gid,"gist_revision":revision,"gist_file_sha256":intent_sha}
complete={"schema":"arc.recovery.archive-complete.v2","freeze_plan_sha256":freeze,"capture_id":capture,"rollout_manifest_sha256":rollout,"source_commit":"1"*40,"archive_manifest_sha256":manifest,"object_count_before_complete":27,"validator_bundle_count":6,"finalization_anchor":anchor}
(r/"COMPLETE.json").write_bytes(c(complete))
response={"id":gid,"public":False,"owner":{"login":"FerrumVir"},"history":[{"version":revision}],"files":{intent["github_anchor_policy"]["filename"]:{"truncated":False,"content":raw.decode()}},"created_at":"2026-08-31T00:00:00Z"}
(r/"historical.json").write_bytes(c(response))
PY
    export ARC_OPERATOR_GH_LOGIN=FerrumVir
    # shellcheck disable=SC2329 # invoked indirectly by the sourced orchestrator
    configure_github_anchor_transport() { :; }
    # shellcheck disable=SC2329 # invoked indirectly by the sourced orchestrator
    gh_api() {
        [ "$#" -eq 1 ] || return 91
        [ "$1" = "/gists/$(printf 'e%.0s' {1..32})/$(printf 'f%.0s' {1..40})" ] || return 92
        cat "$f/historical.json"
    }
    fetch_verify_or_recover_complete_gist_anchor "$f/COMPLETE.json" \
        "$f/protected/finalization.json" "$f/protected/finalization.json.gist-anchor.json" || return 1
    cmp -s "$f/expected-intent.json" "$f/protected/finalization.json" || return 1
    [ -f "$f/protected/finalization.json.sha256" ] || return 1
    [ -f "$f/protected/finalization.json.gist-anchor.json" ] || return 1
)

new_v3_paths_and_post_cutover_source_are_verified() {
    for required in '--new-node-paths' 'os.path.commonpath((old, new))' \
      '--verify-live-captures' 'sealed-source-status' \
      'verify_production_archive(verify_live_captures=True)' \
      'require_prearchive_manifest(manifest)'; do
        grep -Fq -- "$required" "$ORCHESTRATOR" "$NODE_HELPER" "$ROLLOUT" || return 1
    done
    ! grep -Fq 'source_tree_immutable_in_place' "$NODE_HELPER" "$ORCHESTRATOR"
}

legacy_network_quarantine_is_durable_exact_and_precedes_freeze() {
    python3 - "$NODE_HELPER" <<'PY' || return 1
import pathlib, sys
t = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
assert 'table_name = "arc_legacy_maintenance_v1"' in t
assert 'priority = -310' in t
phase = t[t.index("pre_fence_quiesce()") : t.index("verify_exact_writer()")]
runtime = phase.index('stage_prefreeze_runtime_safety')
install = phase.index('install_legacy_network_quarantine')
quarantine_return = phase.index('ARC_RECOVERY_QUARANTINE_ONLY')
freeze = phase.index('fast_cgroup_freeze "$@"')
assert runtime < install < quarantine_return < freeze
stop = t[t.index("stop_node_cleanly()") : t.index("def canonical(value):", t.index("stop_node_cleanly()"))]
assert stop.index('verify_legacy_network_quarantine') < stop.index('commit_restart_barrier')
assert 'network_quarantine_sha256' in t
intent_start = t.index("verify_or_arm_stop_journal()")
intent = t[intent_start : t.index("\narm_stop_journal()", intent_start)]
assert '"network_quarantine_sha256": network_quarantine_sha' in intent
for exact in (
    'iifname != "lo" counter drop', 'oifname != "lo" counter drop',
    'tcp dport 22 counter accept', 'tcp sport 22 counter accept',
    'udp sport 67 udp dport 68', 'udp sport 68 udp dport 67',
    'udp sport 547 udp dport 546', 'udp sport 546 udp dport 547',
    'icmpv6 type {{ 2, 133, 134, 135, 136 }}',
    'After=local-fs.target firewalld.service',
    'Wants=network-pre.target',
    'Before=network-pre.target network.target network-online.target ',
    'arc-self-heal.service arc-node.service arc-node-update.service arc-node-update.timer',
    'f"ExecStartPre={ensure_path}\\n"',
    'DefaultDependencies=no', 'automatic_unfence": False',
):
    assert exact in t, exact
verify = t[t.index("verify_legacy_network_quarantine()") : t.index("quarantine_status()")]
for exact in ('owned_rule_ast_sha256', 'preexisting_firewall_structural_sha256',
              'live network-quarantine normalized AST hash differs from receipt',
              'nonowned firewall changed under network quarantine',
              'network-quarantine nft tool hash differs',
              'rule has unknown attributes', 'expr order/shape differs',
              'exact stateless ruleset hash differs from receipt'):
    assert exact in verify, exact
status = t[t.index("quarantine_status()") : t.index("quarantine_public_cross_proof()")]
for exact in ('listener_inventory', 'loopback_head', 'quarantine_policy', 'rule_counters'):
    assert exact in status
for exact in ('status snapshot contains an unknown AST object',
              'status expr order/shape differs',
              'status semantic AST differs from receipt'):
    assert exact in status
cross = t[t.index("quarantine_public_cross_proof()") : t.index("pre_fence_quiesce()")]
for exact in ('public_info_after_block', 'public_latest_block', 'fenced_head',
              'state_root', 'public_latest_hash_matches', 'after_status="$(quarantine_status',
              'proof["quarantine_status"]=status'):
    assert exact in cross
dispatch = t[t.index('ACTION="${1:-}"'):]
assert 'fence-stop|quarantine|quarantine-starter|quarantine-authority|' in dispatch
assert 'obsolete global quarantine authority is retired; use an exact quarantine round' in dispatch
for exact in ('quarantine-round-authorize)', 'quarantine-round-ready)',
              'quarantine-round-apply)', 'quarantine-round-applied-status)',
              'quarantine-round-precommit-status)', 'quarantine-round-status)',
              'stop-after-quarantine-round)'):
    assert exact in dispatch, exact
assert 'quarantine-status)' in dispatch and '[ "$#" -eq 4 ]' in dispatch
assert 'quarantine-restart-arm)' in dispatch and 'quarantine_restart_arm "$2" "$3" "$4"' in dispatch
assert 'quarantine-restart-status)' in dispatch and 'quarantine_restart_status "$2" "$3" "$4"' in dispatch
assert 'quarantine-monitor-receipt)' in dispatch and 'quarantine_monitor_receipt "$2" "$3" "$4"' in dispatch
assert 'quarantine-public-cross-proof)' in dispatch and '[ "$#" -eq 8 ]' in dispatch
for exact in (
    'arc.recovery.legacy-network-quarantine-monitor.v1',
    'arc.recovery.legacy-network-fence-monitor-contract.v1',
    'arc.recovery.legacy-network-fence-incident-intent.v1',
    'arc.recovery.quarantine-live-restart-arm.v1',
    'arc.recovery.quarantine-detached-supervisor-frozen.v1',
    'arc.recovery.quarantine-live-restart-committed.v1',
    'arc.recovery.quarantine-live-restart-status.v1',
    'ExecStartPre=/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C',
    'continuous_poll_interval_milliseconds',
    'unreviewed enabled firewall loader/mutator',
):
    assert exact in t, exact
arm = t[t.index("quarantine_restart_arm()") : t.index("quarantine_restart_status()")]
assert arm.index('publish(arm_path,arm)') < arm.index('os.unlink(marker.name,dir_fd=parent)')
assert arm.index('os.write(freezer,b"1")') < arm.index('os.unlink(marker.name,dir_fd=parent)')
assert arm.index('os.write(freezer,b"1")') < arm.index('publish(arm_path,arm)')
assert 'Restart":"no"' in arm and 'ConditionPathExists=/etc/arc-recovery/legacy-start-allowed' in arm
assert 'zzzx-arc-recovery-quarantine-arm.conf' in arm and 'ConditionPathExists=!' in arm
assert 'sealed-boot runtime Restart=no drop-in survived reboot' in arm
assert 'historical runtime Restart=no safety hash differs' in arm
monitor = t[t.index("harden_legacy_network_quarantine()") : t.index("verify_legacy_network_quarantine()")]
assert monitor.index('intent_raw=create(intent_path,intent)') < monitor.index('freeze(target)')
assert monitor.index('freeze(target)') < monitor.index('os.unlink(marker.name,dir_fd=parent)')
assert 'RestartPreventExitStatus=77' in t
assert 'delete table inet arc_legacy_maintenance_v1' not in t
controller=t[t.index("def thaw(entry, intent_path):"):t.index("def term_progress(prefix):")]
assert controller.index('check_network_fence()') < controller.index('directory = os.open(')
assert controller.index('check_network_fence()', controller.index('details.st_dev')) < controller.index('os.write(freezer, b"0")')
internal=t[t.index("def check_network_fence():"):t.index("def fsync_dir(path):", t.index("def check_network_fence():"))]
for exact in ('owned_ruleset_stateless_sha256','preexisting_firewall_structural_sha256',
              'network quarantine exact ruleset changed inside stop controller'):
    assert exact in internal
PY
}

persisted_head_is_reexecuted_hash_bound_and_capture_exact() {
    python3 - "$NODE_HELPER" <<'PY' || return 1
import pathlib,sys
t=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
body=t[t.index("persisted_head()") : t.index("stage_input()")]
ordered=[
    'verify_stop_journal_semantics', 'verify_legacy_restart_fence',
    'verify_legacy_network_quarantine', 'verify_capture_source',
    'exec 8<"$binary"', '/proc/self/fd/8 recovery export',
    '[ "$(hash_file /proc/self/fd/14)" = "$wal_before" ]',
]
positions=[body.index(item) for item in ordered]
assert positions==sorted(positions)
assert body.index('verify_capture_source "$capture_root"', positions[-1]) > positions[-1]
for exact in (
    'schema":"arc.recovery.persisted-legacy-head.v1',
    'source_main_commit', 'inspector_binary_sha256',
    'network_quarantine_receipt_sha256', 'capture_source_sha256',
    'source_data_index_sha256', 'state_wal_size', 'snapshot_size',
    'export_status":"EXPORTED_UNSIGNED"',
    '"head":{"height":height,"block_hash":block_hash,"state_root":state_root}',
    'rerun differs from its create-only receipt', 'rerun_reexecutes_export',
    'details.st_nlink!=1', 'stat.S_IMODE(details.st_mode)!=0o400',
    'openat/O_NOFOLLOW FD identity differs', 'export-source/state.wal',
    'candidate.inspect.json', 'inspect_summary_sha256', 'wal_boundary_sha256',
    'os.dup(13)', 'os.dup(12)', 'export summary exact key set differs',
    'snapshot pathname changed after held-FD open',
    'offline-wal-recovery.v2', 'source_file_identity', 'staged_file_contract',
    'complete publication partial differs from re-executed receipt',
    'except (UnicodeError,json.JSONDecodeError): parsed_existing=None',
    'ARC_RECOVERY_PERSISTED_HEAD_FAIL_AT")=="after-write"',
    'ARC_RECOVERY_PERSISTED_HEAD_FAIL_AT',
): assert exact in body, exact
assert 'os.open("/proc/self/fd/13"' not in body
assert 'os.open("/proc/self/fd/12"' not in body
assert body.index('/proc/self/fd/8 recovery export') < body.index('if output.exists()')
dispatch=t[t.index('ACTION="${1:-}"'):]
assert 'persisted-head)' in dispatch and '[ "$#" -eq 9 ]' in dispatch
PY
}

linux_held_fd_openat_is_executable() (
    [ "$(uname -s)" = Linux ] || return 0
    local root
    root="$(mktemp -d)"; trap 'rm -rf -- "$root"' EXIT
    mkdir "$root/data"
    printf 'wal-bytes' > "$root/data/state.wal"
    printf 'snapshot-bytes' > "$root/state.snapshot.lz4"
    exec 12<"$root/state.snapshot.lz4" 13<"$root/data" 14<"$root/data/state.wal"
    python3 - <<'PY'
import os,stat
directory=os.dup(13)
try:
    wal=os.open("state.wal",os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=directory)
    assert (os.fstat(wal).st_dev,os.fstat(wal).st_ino)==(os.fstat(14).st_dev,os.fstat(14).st_ino)
finally:
    os.close(directory)
snapshot=os.dup(12)
try:
    assert stat.S_ISREG(os.fstat(snapshot).st_mode)
    assert os.read(snapshot,100)==b"snapshot-bytes"
finally:
    os.close(snapshot);os.close(wal)
try:
    os.open("/proc/self/fd/13",os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
except OSError:
    pass
else:
    raise SystemExit("Linux unexpectedly allowed the unsafe proc-fd O_NOFOLLOW reopen")
PY
    printf 'replacement' > "$root/replacement"
    rm "$root/state.snapshot.lz4"
    ln -s "$root/replacement" "$root/state.snapshot.lz4"
    if python3 - "$root/state.snapshot.lz4" 2>/dev/null <<'PY'
import os,pathlib,sys
path=pathlib.Path(sys.argv[1]); parent=os.open(path.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try: os.open(path.name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=parent)
finally: os.close(parent)
PY
    then
        return 1
    fi
    rm "$root/state.snapshot.lz4"
    cp "$root/replacement" "$root/state.snapshot.lz4"
    if python3 - "$root/state.snapshot.lz4" 2>/dev/null <<'PY'
import os,pathlib,sys
path=pathlib.Path(sys.argv[1]); parent=os.open(path.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:
    fresh=os.open(path.name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=parent)
    assert (os.fstat(12).st_dev,os.fstat(12).st_ino)==(os.fstat(fresh).st_dev,os.fstat(fresh).st_ino)
finally:
    os.close(parent)
    if 'fresh' in locals(): os.close(fresh)
PY
    then
        return 1
    fi
)

persisted_head_partial_truncations_are_resumable() {
    python3 - <<'PY'
import json
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
payload=canonical({"schema":"arc.recovery.persisted-legacy-head.v1",
                   "completed_at":"2026-08-31T12:34:56Z","head":{"height":99}})
completed=payload.index(b"completed_at")
for cut in (0,1,completed-1,completed+3,completed+30,len(payload)-1):
    partial=payload[:cut]
    try: parsed=json.loads(partial)
    except (UnicodeError,json.JSONDecodeError): parsed=None
    assert parsed is None or canonical(parsed)!=partial
    # Production discards this exact root-owned fixed-name incomplete inode,
    # fsyncs the parent, and publishes the re-executed payload.
    partial=b""
    assert partial==b""
assert json.loads(payload)["completed_at"]=="2026-08-31T12:34:56Z"
PY
}

stateful_fake_nft_systemctl_quarantine_contract() {
    python3 - <<'PY'
import copy
EXACT={"family":"inet","name":"arc_legacy_maintenance_v1","priority":-310,
       "chains":["prerouting","input","forward","output"],
       "rules":["loopback","ssh22","dhcp4","dhcp6","icmpv6-control","deny-all-nonloopback"]}
STEPS=("files","dropins","baseline","enable","apply","receipt")
class FakeHost:
    def __init__(self):
        self.durable=set();self.table=None;self.enabled=False;self.unit_ok=True;self.receipt=False
    def validate(self): return self.table==EXACT
    def install(self,crash=None):
        for step in STEPS:
            if step=="files": self.durable.add("files")
            elif step=="dropins": self.durable.add("dropins")
            elif step=="baseline": self.durable.add("baseline")
            elif step=="enable": self.enabled=True;self.durable.add("enable")
            elif step=="apply":
                if self.table is not None and self.table!=EXACT: raise RuntimeError("conflicting table")
                self.table=copy.deepcopy(EXACT)
            elif step=="receipt":
                if not self.validate(): raise RuntimeError("cannot receipt drift")
                self.receipt=True
            if crash==step: raise InterruptedError(step)
    def reboot(self):
        self.table=None
        if self.enabled and self.unit_ok:self.table=copy.deepcopy(EXACT)
    def legacy_start_allowed(self):
        return "dropins" in self.durable and self.enabled and self.unit_ok and self.validate()
for crash in STEPS:
    host=FakeHost()
    try: host.install(crash)
    except InterruptedError: pass
    host.install()
    assert host.receipt and host.validate() and host.legacy_start_allowed()
host=FakeHost();host.table={"foreign":True}
try:host.install()
except RuntimeError:pass
else:raise AssertionError("foreign nft table was clobbered")
host=FakeHost();host.install();host.reboot();assert host.validate() and host.legacy_start_allowed()
host=FakeHost();host.install();host.unit_ok=False;host.reboot();assert not host.legacy_start_allowed()
host=FakeHost();host.install();host.table["rules"].append("source-match-escape")
assert not host.validate() and not host.legacy_start_allowed()
host=FakeHost();host.install();stateless=copy.deepcopy(host.table)
for packets in (0,1,999):
    counters={"deny":{"packets":packets,"bytes":packets*64}}
    assert host.table==stateless and counters["deny"]["packets"]==packets
def verdict(family,direction,loopback,protocol,sport,dport,icmp_type=None):
    assert family in {"ipv4","ipv6"} and direction in {"input","output","forward"}
    if loopback:return "accept"
    if direction=="forward":return "drop"
    if protocol=="tcp" and ((direction=="input" and dport==22) or (direction=="output" and sport==22)):return "accept"
    if protocol=="udp" and ((direction=="input" and (sport,dport) in {(67,68),(547,546)})
                            or (direction=="output" and (sport,dport) in {(68,67),(546,547)})):return "accept"
    if family=="ipv6" and protocol=="icmpv6" and icmp_type in {2,133,134,135,136}:return "accept"
    return "drop"
for family in ("ipv4","ipv6"):
    for direction in ("input","output","forward"):
        assert verdict(family,direction,False,"tcp",9090,9090)=="drop"
        assert verdict(family,direction,False,"udp",9091,9091)=="drop"
        assert verdict(family,direction,False,"udp",443,443)=="drop"

class LiveGuard:
    def __init__(self,detached):
        self.table=copy.deepcopy(EXACT);self.marker=True;self.writer_live=True
        self.writer_frozen=False;self.supervisor_frozen=False;self.incident=False
        self.detached=detached;self.restart_no=True;self.conditions=True
        self.monitor=True;self.arm_barrier=False;self.interpreter=("root-owned",123,456,"a"*64)
    def validator(self):
        return self.monitor and self.table==EXACT and not self.incident and self.interpreter==("root-owned",123,456,"a"*64)
    def arm(self,crash=None):
        assert self.validator() and self.restart_no and self.conditions and self.writer_live
        if self.detached:self.supervisor_frozen=True
        durable_arm=True;self.arm_barrier=True
        if crash=="before-unlink":raise InterruptedError
        self.marker=False
        assert durable_arm and (self.supervisor_frozen if self.detached else True)
    def drift(self):
        self.incident=True
        self.writer_frozen=True
        if self.detached:self.supervisor_frozen=True
        self.marker=False;self.monitor=False
    def manual_start(self):
        return self.marker and not self.arm_barrier and self.conditions and self.validator()
    def reboot(self):
        self.writer_live=False;self.writer_frozen=False;self.supervisor_frozen=False
        self.table=None
        if self.monitor and not self.incident:self.table=copy.deepcopy(EXACT)
for detached in (False,True):
    guard=LiveGuard(detached)
    try:guard.arm("before-unlink")
    except InterruptedError:pass
    assert guard.marker and not guard.manual_start() and (guard.supervisor_frozen if detached else True)
    guard.arm();assert not guard.marker and not guard.manual_start()
    guard.reboot();assert not guard.writer_live and not guard.manual_start()
    guard=LiveGuard(detached);guard.table["rules"].append("runtime-reload-drift");guard.drift()
    assert guard.incident and guard.writer_frozen and not guard.marker and not guard.manual_start()
guard=LiveGuard(False);guard.interpreter=("swapped",123,456,"b"*64)
assert not guard.validator() and not guard.manual_start()
PY
}

quarantine_retirement_is_one_way_resumable_and_exact() {
    python3 - "$NODE_HELPER" <<'PY' || return 1
import copy,pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
body=text[text.index("quarantine_retire()") : text.index('ACTION="${1:-}"')]
for exact in (
    'arc.recovery.legacy-network-quarantine-retirement-intent.v1',
    'PHASE-01-LEGACY-PUBLIC-RETIRED.json',
    'PHASE-02-FENCE-SERVICE-RETIRED.json',
    'PHASE-03-FENCE-DEPENDENCIES-REMOVED.json',
    'PHASE-04-OWNED-TABLE-REMOVED.json',
    'arc.recovery.legacy-network-quarantine-retirement.v1',
    'rollback_policy": "maintenance-only-no-legacy-restart"',
    'zzzz-arc-recovery-freeze.conf',
    'zzzx-arc-recovery-quarantine-arm.conf',
    'zzzy-arc-recovery-network-fence.conf',
    'subprocess.run([str(pinned_nft), "delete", "table", "inet", table]',
    'if mode == "status":',
    'Status is deliberately read-only',
):
    assert exact in body, exact
positions=[body.index(value) for value in (
    'journal / "INTENT.json"',
    'journal / "PHASE-01-LEGACY-PUBLIC-RETIRED.json"',
    'journal / "PHASE-02-FENCE-SERVICE-RETIRED.json"',
    'journal / "PHASE-03-FENCE-DEPENDENCIES-REMOVED.json"',
    'subprocess.run([str(pinned_nft), "delete", "table", "inet", table]',
    'journal / "PHASE-04-OWNED-TABLE-REMOVED.json"',
)]
positions.append(body.rindex('receipt_path = journal / "RECEIPT.json"'))
assert positions==sorted(positions)
assert 'flush ruleset' not in body
assert 'iptables-restore' not in body
status=body[body.index('if mode == "status":') : body.index('intent_fixed = {')]
for forbidden in ('systemctl("stop"', 'systemctl("disable"', 'os.unlink(',
                  '"delete", "table"', 'stable_publish('):
    assert forbidden not in status, forbidden

PHASES=("intent","nginx","monitor","dependencies","table","receipt")
UNITS=("self-heal","node","update","timer")
EXACT_TABLE={"family":"inet","name":"arc_legacy_maintenance_v1",
             "stateless_sha":"a"*64}
class Host:
    def __init__(self):
        self.table=copy.deepcopy(EXACT_TABLE);self.nonowned="b"*64
        self.monitor_active=True;self.monitor_enabled=True;self.nginx=True
        self.dependencies=set(UNITS);self.legacy_barriers=set(UNITS)
        self.arm_barriers=set(UNITS);self.marker=False;self.records=[]
        self.receipt=False;self.legacy_started=False
    def validate_sources(self):
        if self.table!=EXACT_TABLE or self.nonowned!="b"*64:
            raise RuntimeError("drift")
        assert not self.marker and self.legacy_barriers==set(UNITS)
        assert self.arm_barriers==set(UNITS)
    def retire(self,crash=None):
        if "intent" not in self.records:self.validate_sources()
        for step in PHASES:
            if step in self.records:continue
            if step=="intent":pass
            elif step=="nginx":self.nginx=False
            elif step=="monitor":self.monitor_active=False;self.monitor_enabled=False
            elif step=="dependencies":self.dependencies.clear()
            elif step=="table":
                if self.table is not None:
                    if self.table!=EXACT_TABLE or self.nonowned!="b"*64:
                        raise RuntimeError("refuse foreign state")
                    self.table=None
            elif step=="receipt":
                assert self.safe();self.receipt=True
            if crash==step:raise InterruptedError(step)
            self.records.append(step)
    def safe(self):
        return (not self.nginx and not self.monitor_active and not self.monitor_enabled
                and not self.dependencies and self.table is None and not self.marker
                and self.legacy_barriers==set(UNITS) and self.arm_barriers==set(UNITS))
    def status(self):
        snapshot=copy.deepcopy(self.__dict__)
        if not self.receipt or not self.safe() or self.nonowned!="b"*64:
            raise RuntimeError("status failed")
        assert snapshot==self.__dict__
        return snapshot
    def manual_legacy_start(self):
        self.legacy_started=self.marker
        return self.legacy_started

for crash in PHASES:
    host=Host()
    try:host.retire(crash)
    except InterruptedError:pass
    host.retire();assert host.receipt and host.safe() and not host.manual_legacy_start()
    before=copy.deepcopy(host.__dict__);host.status();assert before==host.__dict__
host=Host();host.table={"foreign":True}
try:host.retire()
except RuntimeError:pass
else:raise AssertionError("foreign table was deleted")
host=Host();host.nonowned="c"*64
try:host.retire()
except RuntimeError:pass
else:raise AssertionError("nonowned firewall drift was accepted")
host=Host();host.retire();host.marker=True
try:host.status()
except RuntimeError:pass
else:raise AssertionError("legacy restart marker was accepted after retirement")
PY
}

shared_inputs_stream_without_work_root_materialization() (
    # shellcheck source=/dev/null
    . "$ORCHESTRATOR" >/dev/null
    local root source catalog status expected required available source_size
    # Keep the mutable sparse-file fixture outside the watched Git worktree.
    # Codex/editor/indexer processes are entitled to inspect that worktree and
    # can attach metadata/xattrs to newly discovered large files; the
    # production streaming contract correctly treats the resulting ctime
    # change as source mutation.  This fixture tests our own writes, so use a
    # private temporary directory instead of racing unrelated repo watchers.
    root="$(mktemp -d "${TMPDIR:-/tmp}/arc-archive-stream-test.XXXXXX")"
    trap 'chmod -R u+w "$root" 2>/dev/null || true; rm -rf -- "$root"' EXIT
    chmod 700 "$root"
    mkdir -m 700 "$root/catalog" "$root/work"
    source="$root/sizable-sparse.bin"
    python3 - "$source" <<'PY' || return 1
import os,sys
fd=os.open(sys.argv[1],os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
try:
    os.ftruncate(fd,64*1024*1024)
    os.fsync(fd)
finally:os.close(fd)
PY
    expected="$(hash_file "$source")"
    register_shared_input "$source" "$expected" "$root/catalog" sizable-sparse.bin || return 1
    catalog="$root/catalog/sizable-sparse.bin"
    # GNU stat accepts `-f` as filesystem-statistics mode and can emit output
    # for the valid operand even while failing the BSD `%z` operand.  Try the
    # unambiguous GNU file-format form first; BSD stat rejects `-c` and falls
    # through without contaminating the command substitution.
    [ "$(stat -c '%s' "$catalog" 2>/dev/null || stat -f '%z' "$catalog")" -lt 4096 ] || return 1
    status="$root/work/stream.hash-size"
    stream_shared_input_descriptor "$catalog" "$status" >/dev/null || return 1
    [ "$(cat "$status")" = "$expected 67108864" ] || return 1
    printf X | dd of="$source" bs=1 seek=0 conv=notrunc status=none
    local changed_status
    set +e
    stream_shared_input_descriptor "$catalog" "$root/work/changed.hash-size" >/dev/null 2>&1
    changed_status=$?
    set -e
    [ "$changed_status" -ne 0 ] || return 1

    # Capacity depends only on bounded metadata scratch, not apparent source
    # size. This sparse input is deliberately larger than current free space,
    # which the former total+8GiB reservation could never admit.
    python3 - "$source" "$root" <<'PY' || return 1
import json,os,pathlib,sys
source=pathlib.Path(sys.argv[1]);root=pathlib.Path(sys.argv[2])
fs=os.statvfs(root);available=fs.f_bavail*fs.f_frsize
os.truncate(source,available+16*1024**3)
manifest=root/"manifest.json"
manifest.write_text(json.dumps({"artifacts":{"sparse":{"path":str(source)}}})+"\n")
(root/"manifest.json.sha256").write_text("sidecar\n")
print(available,source.stat().st_size)
PY
    read -r available source_size < <(python3 - "$root" "$source" <<'PY'
import os,pathlib,sys
root=pathlib.Path(sys.argv[1]);source=pathlib.Path(sys.argv[2]);fs=os.statvfs(root)
print(fs.f_bavail*fs.f_frsize,source.stat().st_size)
PY
    )
    [ "$source_size" -gt "$available" ] || return 1
    required="$(verify_archive_work_root_capacity "$root" "$root/manifest.json")" || return 1
    [ "$required" -lt "$available" ] && [ "$required" -lt "$source_size" ] || return 1
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
t=pathlib.Path(sys.argv[1]).read_text();seal=t[t.index("seal_phase()"):]
assert 'cp -- "$source"' not in t
assert 'rclone copy "$shared_root"' not in seal
assert 'stream_shared_input_to_drive "$shared_descriptor"' in seal
assert 'required = total + 8 * 1024**3' not in t
assert 'arc.recovery.shared-input-source.v1' in t
PY
)

archive_scripts_are_lintable() {
    bash -n "$NODE_HELPER" "$ORCHESTRATOR" &&
        PYTHONDONTWRITEBYTECODE=1 python3 -m py_compile \
            "$ROLLOUT" "$FREEZE_MODULE" "$FREEZE_MODULE_TEST" &&
        PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
            "$REPO_ROOT/scripts/recovery/test_recovery_rollout.py" >/dev/null &&
        PYTHONDONTWRITEBYTECODE=1 python3 "$FREEZE_MODULE_TEST" >/dev/null &&
        PYTHONDONTWRITEBYTECODE=1 python3 \
            "$REPO_ROOT/scripts/recovery/test_archive_node_quarantine_runtime.py" \
            >/dev/null &&
        PYTHONDONTWRITEBYTECODE=1 python3 \
            "$REPO_ROOT/scripts/recovery/test_archive_node_prepare_runtime.py" \
            >/dev/null &&
        PYTHONDONTWRITEBYTECODE=1 python3 \
            "$REPO_ROOT/scripts/recovery/test_quarantine_rounds.py" >/dev/null &&
        PYTHONDONTWRITEBYTECODE=1 python3 \
            "$REPO_ROOT/scripts/recovery/test_community_reward_probe.py" >/dev/null &&
        python3 -m json.tool "$SCHEMA" >/dev/null &&
        shellcheck -S warning "$NODE_HELPER" "$ORCHESTRATOR"
}

run_test 'exact authorizations bind every domain' exact_authorizations_bind_every_domain
run_test 'capture id and destination fail closed' capture_id_and_destination_fail_closed
run_test 'timer units normalize an empty MainPID to zero' timer_units_with_empty_mainpid_normalize_to_zero
run_test 'sealed stake proof never claims global halt' sealed_stake_quorum_never_claims_global_halt
run_test 'all six exact writers stop before capture' all_six_exact_writers_stop_before_content_capture
run_test 'late public height sampling is plan-safe and create-only' late_public_height_sampling_is_plan_safe_and_create_only
run_test 'freeze-plan reuse is root-safe and crash-resumable' freeze_plan_install_reuse_is_root_safe_and_crash_resumable
run_test 'offline-stop roots are remote-derived and archive-bound' offline_stop_roots_are_remote_derived_and_archive_bound
run_test 'offline-stop receipt is canonical, private, and adversarial' offline_stop_receipt_is_canonical_private_and_adversarial
run_test 'ordinary and challenged stopped-status execute' ordinary_and_challenged_stopped_status_execute
run_test 'in-place capture detects source tamper' content_capture_fixture_detects_source_tamper
run_test 'partial retry ownership rejects attacks' partial_retry_ownership_rejects_symlink_and_foreign_marker
run_test 'live observations are bounded and immutable' live_observations_are_bounded_create_only_and_resumable
run_test 'disk peak allows growth and reserves v3' disk_peak_allows_growth_and_reserves_v3
run_test 'WAL boundary hashes without duplicates' wal_boundary_hashes_without_duplicate_files
run_test 'model bytes and shards are bound' model_size_hash_and_shards_are_bound
run_test 'archive stream makes no full copy' stream_has_no_full_copy_or_model_member
run_test 'v5 freeze transaction is fault-closed' v5_freeze_transaction_is_fault_closed
run_test 'v5 stop journal semantics are fault-closed' v5_stop_journal_semantics_are_fault_closed
run_test 'classification requires each node once' classification_requires_each_node_once
run_test 'capture readiness resumes exact stopped state' capture_readiness_resumes_stopped_and_indexed_nodes
run_test 'stale freeze capacity cannot cross current readiness gate' stale_freeze_capacity_cannot_cross_current_readiness_gate
run_test 'fleet observation retry rejects any stopped writer' fleet_live_observation_retry_rejects_any_stopped_writer
run_test 'same-generation observation selection resume is byte-identical' live_observation_selection_resume_is_byte_identical
run_test 'mutation dispatch publication is no-replace and crash-resumable' mutation_dispatch_publication_is_no_replace_and_resumable
run_test 'readiness without dispatch does not bind a stale selection' readiness_without_dispatch_does_not_bind_stale_selection
run_test 'all local create-only post-link crashes heal before resume' local_create_only_post_link_crashes_are_reconciled_before_resume
run_test 'positive round result is byte-identical after both publication crash windows' positive_round_result_resume_is_byte_identical
run_test 'positive partial waits for late sixth status and resumes before prefix copy' positive_partial_waits_for_late_transition_before_sealing
run_test 'sealed partial resume skips old-attempt status and preserves exact bytes' sealed_partial_resume_skips_old_attempt_status
run_test 'zero-progress result remains attempt-local and never enters immutable prefix' zero_progress_result_never_enters_immutable_prefix
run_test 'released zero-progress attempt does not bind rotated selection' released_zero_progress_attempt_does_not_bind_rotated_selection
run_test 'remote zero-progress heals only reviewed publish orphans' remote_zero_progress_heals_only_reviewed_publish_orphans
run_test 'capture lock and monotonic lease are portable and bound' capture_lock_and_monotonic_lease_are_portable_and_bound
run_test 'canonical reference is independently required' reference_pair_is_independent_of_final_capture_classes
run_test 'remote COMPLETE rejects object attacks' remote_complete_rejects_missing_tampered_extra
run_test 'verify-complete plan cleanup is total and SSH-free' verify_complete_plan_cleans_transport_state_and_never_uses_ssh
run_test 'archive command scopes clean plan, failure, and nested success state' archive_command_scopes_clean_plan_failure_and_nested_success
run_test 'archive dispatcher preserves phase errexit and completed takeover' archive_dispatcher_preserves_errexit_and_accepts_completed_takeover
run_test 'archive dispatcher signals stop the full phase group and clean' archive_dispatcher_signals_stop_the_full_phase_group_and_clean
run_test 'COMPLETE is last and fully verified' complete_is_last_and_fully_verified
run_test 'immutable Gist revision recovers a lost local intent after latest edit' gist_revision_recovers_lost_local_intent_after_latest_edit
run_test 'new v3 paths preserve frozen source' new_v3_paths_and_post_cutover_source_are_verified
run_test 'legacy network quarantine is durable, exact, and pre-freeze' legacy_network_quarantine_is_durable_exact_and_precedes_freeze
run_test 'persisted head is reexecuted, hash-bound, and capture-exact' persisted_head_is_reexecuted_hash_bound_and_capture_exact
run_test 'Linux held FDs use executable dup and openat semantics' linux_held_fd_openat_is_executable
run_test 'persisted-head truncated partials resume at every completed-at offset' persisted_head_partial_truncations_are_resumable
run_test 'stateful fake nft/systemctl quarantine crash matrix is fail-closed' stateful_fake_nft_systemctl_quarantine_contract
run_test 'quarantine retirement is exact, one-way, read-only on status, and crash-resumable' quarantine_retirement_is_one_way_resumable_and_exact
run_test 'shared archive inputs stream without work-root materialization' shared_inputs_stream_without_work_root_materialization
run_test 'archive scripts pass syntax, lint, and embedded runtime suites' archive_scripts_are_lintable
finish_tests

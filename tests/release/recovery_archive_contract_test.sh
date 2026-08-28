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

exact_authorizations_bind_every_domain() {
    for required in 'expected_go="FREEZE $freeze_sha CAPTURE $capture_id"' \
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
assert b.index('ensure_stopped "$freeze_plan" "$freeze_sha" "$capture_id" nyc')<b.index('for node in "${REMAINING[@]}"')
assert b.index('for node in "${REMAINING[@]}"')<b.index('ensure_offline_capture "$capture_id" "$node"')
assert 'ALL SIX CONTROLLED WRITERS HALTED' in b and 'no global halt is claimed' in b
PY
    ! grep -Eq 'pkill[[:space:]]|killall[[:space:]]|kill[[:space:]]+-9' "$NODE_HELPER"
}

content_capture_fixture_detects_source_tamper() (
    # shellcheck source=/dev/null
    . "$NODE_HELPER" >/dev/null
    local f c d id n; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    c="$f/capture"; d="$f/legacy-data"; id="$(printf 'a%.0s' {1..64})"; n=nyc
    mkdir -p "$c" "$d"; printf sealed-wal > "$d/state.wal"; printf state > "$d/state.bin"
    write_regular_tree_inventory "$d" "$c/source-data.files.sha256" || return 1
    python3 - "$d" "$c/capture-source.json" <<'PY' || return 1
import hashlib,json,pathlib,sys
r,o=map(pathlib.Path,sys.argv[1:]); w=r/"state.wal"; files=[p for p in r.rglob("*") if p.is_file()]
v={"schema":"arc.recovery.capture-source.v1","data_dir":str(r),"data_device":r.stat().st_dev,"data_inode":r.stat().st_ino,"data_bytes":sum(p.stat().st_size for p in files),"data_files":len(files),"state_wal_bytes":w.stat().st_size,"state_wal_sha256":hashlib.sha256(w.read_bytes()).hexdigest(),"external_snapshots":[]}
o.write_text(json.dumps(v,sort_keys=True,separators=(",",":"))+"\n")
PY
    printf 'capture_id=%s\nnode=%s\n' "$id" "$n" > "$c/capture.inventory"
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

helper_and_writer_identity_are_toctou_safe() {
    for r in 'mktemp "$root/upload.XXXXXX"' 'exec 9<"$helper"' 'sha256sum /proc/self/fd/9' 'exec /proc/self/fd/9 "$@"' 'writer PID was reused or restarted after audit' 'writer argv differs from sealed audit' 'supervisor MainPID differs from sealed audit'; do grep -Fq -- "$r" "$ORCHESTRATOR" "$NODE_HELPER" || return 1; done
    ! grep -Eq 'pkill[[:space:]]|exec[[:space:]]+"\$helper"' "$ORCHESTRATOR" "$NODE_HELPER" || return 1
    grep -Fq 'legacy-start authorization marker exists' "$NODE_HELPER" || return 1
    grep -Fq 'Never ask systemd to stop the audited writer' "$NODE_HELPER" || return 1
    ! grep -Eq 'disable[[:space:]]+--now[[:space:]].*(arc-node|arc-self-heal)' "$NODE_HELPER"
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
    # shellcheck disable=SC2329
    host_for() { printf '%s\n' "$1"; }
    # Fixture override invoked indirectly by sourced remote_readiness.
    # shellcheck disable=SC2329
    freeze_node_field() {
        case "$3" in
            writer_pid|writer_start_ticks|supervisor_main_pid|stake) printf '1\n' ;;
            boot_id) printf '00000000-0000-0000-0000-000000000000\n' ;;
            supervisor_unit) printf 'arc-node.service\n' ;;
            executable_path|data_dir|model_path) printf '/safe/%s/%s\n' "$2" "$3" ;;
            executable_sha256|argv_sha256|model_sha256|validator_address) printf 'a%.0s' {1..64}; printf '\n' ;;
            model_size_bytes) printf '4081004224\n' ;;
            *) return 1 ;;
        esac
    }
    # Fixture override invoked indirectly by sourced remote_readiness.
    # shellcheck disable=SC2329
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
    # shellcheck disable=SC2329
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
    local f remote archive_sha complete_sha sums_sha; f="$(mktemp -d)"; trap 'rm -rf -- "$f"' EXIT
    remote="$f/remote"; mkdir -p "$f/shared" "$f/meta" "$f/complete" "$remote" "$f/bin"; printf alpha > "$f/shared/a.txt"
    python3 - "$f" <<'PY' || return 1
import hashlib,json,pathlib,sys
r=pathlib.Path(sys.argv[1]); d=r/"remote"; s=r/"shared"; rows=[]; cs=["valid_noncanonical_fork"]*3+["preserved_unclassified"]*3
h=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for name in ("arc-node","genesis.toml","validator-public-keys.json","legacy-validator-set-40m.json","source.snapshot.lz4","source.state.wal","recovery.arcchkpt","archive-fleet-to-drive.sh","archive-node.sh","recovery_rollout.py","recovery-manifest.schema.json"):
 (s/name).write_bytes((name+"-bytes").encode())
obj=lambda name:{"name":name,"size":(s/name).stat().st_size,"sha256":h(s/name)}
ref={"schema":"arc.recovery.canonical-reference.v1","independently_verified":True,"allow_unbound_legacy_wal":False,"verifier_binary":obj("arc-node"),"genesis":obj("genesis.toml"),"validator_public_keys":obj("validator-public-keys.json"),"legacy_validator_set":obj("legacy-validator-set-40m.json"),"source_snapshot":obj("source.snapshot.lz4"),"source_wal":obj("source.state.wal"),"selected_checkpoint":obj("recovery.arcchkpt"),"source_height":137145,"source_block_hash":"1"*64,"source_state_root":"2"*64,"transition_state_root":"3"*64,"checkpoint_manifest_hash":"4"*64,"source_consensus_round":7,"created_at_unix_ms":8,"recovery_epoch":9,"validator_set_id":10}
(s/"canonical-reference.json").write_text(json.dumps(ref,sort_keys=True,separators=(",",":"))+"\n")
(s/"archive-seal-options.json").write_text('{"allow_unbound_legacy_wal":false}\n')
for n,c in zip(("nyc","lax","ams","lhr","nrt","sgp"),cs):
 b=d/f"legacy-{n}.tar.zst"; b.write_bytes((n+"-bundle").encode()); i=d/f"legacy-{n}.inventory"; i.write_text(n+"-inventory\n"); bs=d/(b.name+".sha256"); bs.write_text(f"{h(b)}  {b.name}\n"); ins=d/(i.name+".sha256"); ins.write_text(f"{h(i)}  {i.name}\n"); rows.append({"schema":"arc.recovery.bundle-status.v1","capture_id":"b"*64,"node":n,"rollout_manifest_sha256":"c"*64,"classification":c,"bundle":{"name":b.name,"size":b.stat().st_size,"sha256":h(b),"sidecar_name":bs.name,"sidecar_sha256":h(bs)},"inventory":{"name":i.name,"size":i.stat().st_size,"sha256":h(i),"sidecar_name":ins.name,"sidecar_sha256":h(ins)}})
(r/"statuses.jsonl").write_text("".join(json.dumps(x,sort_keys=True,separators=(",",":"))+"\n" for x in rows))
PY
    archive_sha="$(build_archive_metadata "$f/shared" "$f/statuses.jsonl" "$f/meta" "$f/complete" "$(printf 'a%.0s' {1..64})" "$(printf 'b%.0s' {1..64})" "$(printf 'c%.0s' {1..64})" "$(printf '1%.0s' {1..40})" "$(hash_file "$f/shared/archive-fleet-to-drive.sh")" "$(hash_file "$f/shared/archive-node.sh")" "$(hash_file "$f/shared/recovery_rollout.py")" "$(hash_file "$f/shared/recovery-manifest.schema.json")" 0 3 3)" || return 1
    cp "$f/shared/"* "$remote/"; cp "$f/meta/"* "$remote/"; cp "$f/complete/COMPLETE.json" "$remote/"
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
    PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" "" "" "" "$complete_sha" "$archive_sha" "$sums_sha" "$(printf 'c%.0s' {1..64})" >/dev/null || return 1
    printf x >> "$remote/canonical-reference.json"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    cp "$f/shared/canonical-reference.json" "$remote/canonical-reference.json"
    mv "$remote/legacy-nyc.tar.zst" "$f/missing"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    mv "$f/missing" "$remote/legacy-nyc.tar.zst"; printf x >> "$remote/legacy-nyc.tar.zst"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    printf 'nyc-bundle' > "$remote/legacy-nyc.tar.zst"; printf extra > "$remote/EXTRA"; ( PATH="$f/bin:$PATH" verify_remote_complete "local:$remote" ) >/dev/null 2>&1 && return 1
    return 0
)

complete_is_last_and_fully_verified() {
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib,sys
t=pathlib.Path(sys.argv[1]).read_text(); t=t[t.index("seal_phase()"):]
m=['stream_bundle_to_drive "$node"','build_archive_metadata \\','rclone copy "$metadata_root"','rclone check "$metadata_root"','upload_immutable "$complete_root/COMPLETE.json"','verify_remote_complete "$destination"']; p=[t.index(x) for x in m[:-1]]+[t.rindex(m[-1])]; assert p==sorted(p); assert 'existing COMPLETE.json fully verified; verification-only resume' in t
PY
}

new_v3_paths_and_post_cutover_source_are_verified() {
    for required in '--new-node-paths' 'os.path.commonpath((old, new))' \
      '--verify-live-captures' 'sealed-source-status' \
      'verify_production_archive(verify_live_captures=True)' \
      'require_prearchive_manifest(manifest)'; do
        grep -Fq -- "$required" "$ORCHESTRATOR" "$NODE_HELPER" "$ROLLOUT" || return 1
    done
    ! grep -Fq 'source_tree_immutable_in_place' "$NODE_HELPER" "$ORCHESTRATOR"
}

archive_scripts_are_lintable() { bash -n "$NODE_HELPER" "$ORCHESTRATOR" && PYTHONDONTWRITEBYTECODE=1 python3 -m py_compile "$ROLLOUT" && PYTHONDONTWRITEBYTECODE=1 python3 -m unittest "$REPO_ROOT/scripts/recovery/test_recovery_rollout.py" >/dev/null && python3 -m json.tool "$SCHEMA" >/dev/null && shellcheck -S warning "$NODE_HELPER" "$ORCHESTRATOR"; }

run_test 'exact authorizations bind every domain' exact_authorizations_bind_every_domain
run_test 'capture id and destination fail closed' capture_id_and_destination_fail_closed
run_test 'sealed stake proof never claims global halt' sealed_stake_quorum_never_claims_global_halt
run_test 'all six exact writers stop before capture' all_six_exact_writers_stop_before_content_capture
run_test 'in-place capture detects source tamper' content_capture_fixture_detects_source_tamper
run_test 'partial retry ownership rejects attacks' partial_retry_ownership_rejects_symlink_and_foreign_marker
run_test 'disk peak allows growth and reserves v3' disk_peak_allows_growth_and_reserves_v3
run_test 'WAL boundary hashes without duplicates' wal_boundary_hashes_without_duplicate_files
run_test 'model bytes and shards are bound' model_size_hash_and_shards_are_bound
run_test 'archive stream makes no full copy' stream_has_no_full_copy_or_model_member
run_test 'helper and writer identity are TOCTOU-safe' helper_and_writer_identity_are_toctou_safe
run_test 'classification requires each node once' classification_requires_each_node_once
run_test 'capture readiness resumes exact stopped state' capture_readiness_resumes_stopped_and_indexed_nodes
run_test 'canonical reference is independently required' reference_pair_is_independent_of_final_capture_classes
run_test 'remote COMPLETE rejects object attacks' remote_complete_rejects_missing_tampered_extra
run_test 'COMPLETE is last and fully verified' complete_is_last_and_fully_verified
run_test 'new v3 paths preserve frozen source' new_v3_paths_and_post_cutover_source_are_verified
run_test 'archive scripts pass syntax and lint' archive_scripts_are_lintable
finish_tests

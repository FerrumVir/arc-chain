#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

ASSEMBLER="$REPO_ROOT/scripts/release/assemble-release.sh"
CUTOVER_FIXTURE_BUILDER="$TEST_DIR/make_cutover_release_fixture.py"
CUTOVER_ASSET_DERIVER="$REPO_ROOT/scripts/release/assemble-cutover-assets.py"
CANONICAL_HEADLESS_ASSETS='
arc-node-linux-x86_64
arc-cli-linux-x86_64
arc-node-linux-arm64
arc-cli-linux-arm64
arc-node-macos-arm64
arc-cli-macos-arm64
arc-node-macos-x86_64
arc-cli-macos-x86_64
arc-node-windows-x86_64.exe
arc-cli-windows-x86_64.exe
'

ASSEMBLY_SANDBOXES=""
NEW_ASSEMBLY_SANDBOX=""
cleanup_assembly_sandboxes() {
    local sandbox
    for sandbox in $ASSEMBLY_SANDBOXES; do
        [ -n "$sandbox" ] && rm -rf "$sandbox"
    done
}
trap cleanup_assembly_sandboxes EXIT

write_nonempty() {
    local destination="$1" content="${2:-fixture}"
    mkdir -p "$(dirname "$destination")"
    printf '%s\n' "$content" >"$destination"
}

derive_cutover_fixture() {
    local full_handoff="$1" public_handoff="$2" binary="$3" genesis="$4"
    mkdir -p "$public_handoff"
    python3 "$CUTOVER_ASSET_DERIVER" \
        --handoff-dir "$full_handoff" \
        --output-dir "$public_handoff" \
        --verifier-binary "$binary" \
        --inspector-binary "$binary" \
        --genesis "$genesis" \
        --repository FerrumVir/arc-chain \
        --tag v0.8.0 \
        --commit 9999999999999999999999999999999999999999
}

new_assembly_fixture() {
    local sandbox artifacts asset
    sandbox="$(mktemp -d "${TMPDIR:-/tmp}/arc-release-assembly.XXXXXX")"
    ASSEMBLY_SANDBOXES="$ASSEMBLY_SANDBOXES $sandbox"
    artifacts="$sandbox/artifacts"
    mkdir -p "$artifacts" "$sandbox/output"

    for asset in $CANONICAL_HEADLESS_ASSETS; do
        write_nonempty "$artifacts/headless/$asset" "binary fixture: $asset"
    done

    write_nonempty "$artifacts/arc-desktop-macos-arm64/ARC.Node.app.tar.gz"
    write_nonempty "$artifacts/arc-desktop-macos-arm64/ARC.Node.app.tar.gz.sig" 'mac-arm-signature'
    write_nonempty "$artifacts/arc-desktop-macos-arm64/ARC.Node.dmg"

    write_nonempty "$artifacts/arc-desktop-macos-x86_64/ARC.Node.app.tar.gz"
    write_nonempty "$artifacts/arc-desktop-macos-x86_64/ARC.Node.app.tar.gz.sig" 'mac-intel-signature'
    write_nonempty "$artifacts/arc-desktop-macos-x86_64/ARC.Node.dmg"

    write_nonempty "$artifacts/arc-desktop-windows-x86_64/ARC.Node_0.8.0_x64-setup.exe"
    write_nonempty "$artifacts/arc-desktop-windows-x86_64/ARC.Node_0.8.0_x64-setup.exe.sig" 'windows-signature'
    write_nonempty "$artifacts/arc-desktop-windows-x86_64/ARC.Node_0.8.0_x64.msi"

    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node_0.8.0_amd64.AppImage"
    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node_0.8.0_amd64.AppImage.sig" 'linux-signature'
    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node_0.8.0_amd64.deb"
    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node-0.8.0-1.x86_64.rpm"

    if ! python3 "$CUTOVER_FIXTURE_BUILDER" \
        --handoff-dir "$sandbox/cutover-full-handoff" \
        --binary "$artifacts/headless/arc-node-linux-x86_64" \
        --genesis "$REPO_ROOT/genesis.toml"; then
        printf 'failed to create deterministic cutover release fixture\n'
        return 1
    fi
    derive_cutover_fixture \
        "$sandbox/cutover-full-handoff" \
        "$sandbox/cutover-handoff" \
        "$artifacts/headless/arc-node-linux-x86_64" \
        "$REPO_ROOT/genesis.toml" >/dev/null || return 1

    NEW_ASSEMBLY_SANDBOX="$sandbox"
}

run_assembler() {
    local sandbox="$1" release_tag="${2:-v0.8.0}"
    local genesis_file="${3:-$REPO_ROOT/genesis.toml}"
    local cutover_handoff="$sandbox/cutover-handoff"
    if [ "$genesis_file" != "$REPO_ROOT/genesis.toml" ]; then
        local cutover_run full_handoff
        cutover_run="$(mktemp -d "$sandbox/cutover-run.XXXXXX")"
        full_handoff="$cutover_run/full-handoff"
        cutover_handoff="$cutover_run/handoff"
        python3 "$CUTOVER_FIXTURE_BUILDER" \
            --handoff-dir "$full_handoff" \
            --binary "$sandbox/artifacts/headless/arc-node-linux-x86_64" \
            --genesis "$genesis_file" || return 1
        derive_cutover_fixture \
            "$full_handoff" \
            "$cutover_handoff" \
            "$sandbox/artifacts/headless/arc-node-linux-x86_64" \
            "$genesis_file" >/dev/null || return 1
    fi
    (
        cd "$REPO_ROOT" || exit 1
        env \
            ARTIFACTS_DIR="$sandbox/artifacts" \
            OUTPUT_DIR="$sandbox/output" \
            GENESIS_FILE="$genesis_file" \
            CUTOVER_HANDOFF_DIR="$cutover_handoff" \
            RELEASE_TAG="$release_tag" \
            RELEASE_COMMIT='9999999999999999999999999999999999999999' \
            RELEASE_DATE='2026-08-26T12:00:00Z' \
            REPOSITORY='FerrumVir/arc-chain' \
            /bin/bash "$ASSEMBLER"
    )
}

complete_fixture_produces_verifiable_contract() {
    local sandbox asset expected_count actual_count filename
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    if ! run_assembler "$sandbox" >"$sandbox/assemble.out" 2>&1; then
        sed -n '1,160p' "$sandbox/assemble.out"
        return 1
    fi

    for asset in $CANONICAL_HEADLESS_ASSETS; do
        if [ ! -s "$sandbox/output/$asset" ]; then
            printf 'assembled release is missing canonical headless asset: %s\n' "$asset"
            return 1
        fi
    done
    [ -s "$sandbox/output/SHA256SUMS" ] || {
        printf 'assembled release has no SHA256SUMS\n'
        return 1
    }

    expected_count="$(find "$sandbox/output" -maxdepth 1 -type f ! -name SHA256SUMS | wc -l | tr -d ' ')"
    actual_count="$(awk '$1 ~ /^[0-9a-f]{64}$/ && NF == 2 { count += 1 } END { print count + 0 }' "$sandbox/output/SHA256SUMS")"
    assert_equals "$expected_count" "$actual_count" 'SHA256SUMS must cover every other published file exactly once' || return 1
    assert_equals 31 "$(find "$sandbox/output" -maxdepth 1 -type f | wc -l | tr -d ' ')" \
        'unsigned release must have the exact 31-file allowlist' || return 1

    while read -r _hash filename; do
        case "$_hash" in \#) continue ;; esac
        if [ ! -s "$sandbox/output/$filename" ]; then
            printf 'SHA256SUMS names a missing/empty file: %s\n' "$filename"
            return 1
        fi
    done <"$sandbox/output/SHA256SUMS"

    assert_file_contains "$sandbox/output/SHA256SUMS" '^# ARC release manifest v1$' \
        'signed manifest header omits its schema' || return 1
    assert_file_contains "$sandbox/output/SHA256SUMS" '^# repository=FerrumVir/arc-chain$' \
        'signed manifest header omits its repository binding' || return 1
    assert_file_contains "$sandbox/output/SHA256SUMS" '^# tag=v0\.8\.0$' \
        'signed manifest header omits its tag binding' || return 1
    assert_file_contains "$sandbox/output/SHA256SUMS" '^# commit=9{40}$' \
        'signed manifest header omits its commit binding' || return 1

    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$sandbox/output" && sha256sum -c SHA256SUMS >/dev/null) || return 1
    else
        (cd "$sandbox/output" && shasum -a 256 -c SHA256SUMS >/dev/null) || return 1
    fi

    assert_file_contains "$sandbox/output/latest.json" '"version":[[:space:]]*"0\.8\.0"' \
        'latest.json does not carry the exact tag version' || return 1
    assert_file_not_contains "$sandbox/output/latest.json" 'releases/latest/download' \
        'latest.json payload URLs must point at the exact release tag' || return 1
    assert_file_contains "$sandbox/output/latest.json" '/releases/download/v0\.8\.0/' \
        'latest.json has no exact-tag artifact URL' || return 1
    assert_file_contains "$sandbox/output/genesis.toml" \
        '^validator_set_complete[[:space:]]*=[[:space:]]*true$' \
        'release did not preserve the approved complete validator set' || return 1
    assert_file_contains "$sandbox/output/genesis.toml" '^\[\[validators\]\]' \
        'release genesis omitted the approved validator set' || return 1
    assert_file_contains "$sandbox/output/genesis.toml" \
        '^community_rewards_v1_activation_height[[:space:]]*=[[:space:]]*137146$' \
        'release did not preserve the checkpoint-bound reward activation height' || return 1
    for filename in \
        arc-legacy-maintenance-boundary.json \
        arc-recovery-checkpoint-descriptor.json \
        arc-cutover-policy.json; do
        [ -s "$sandbox/output/$filename" ] || {
            printf 'assembled release is missing cutover asset: %s\n' "$filename"
            return 1
        }
        assert_file_contains "$sandbox/output/SHA256SUMS" \
            "[[:space:]]$filename$" \
            "owner-signed manifest omits cutover asset $filename" || return 1
    done
    [ ! -e "$sandbox/output/arc-recovery-checkpoint.arcchkpt" ] || {
        printf 'full protected checkpoint was copied into the public release\n'
        return 1
    }
    python3 - \
        "$sandbox/output/arc-recovery-checkpoint-descriptor.json" \
        "$sandbox/output/arc-cutover-policy.json" <<'PY' || return 1
import hashlib, json, pathlib, re, sys
descriptor_path, policy_path = map(pathlib.Path, sys.argv[1:])
descriptor_raw = descriptor_path.read_bytes()
policy_raw = policy_path.read_bytes()
descriptor = json.loads(descriptor_raw)
policy = json.loads(policy_raw)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
assert descriptor_raw == canonical(descriptor)
assert policy_raw == canonical(policy)
assert len(descriptor_raw) <= 1024 * 1024
assert set(descriptor) == {
    "approved_validators", "canonical_inspection", "capture_id", "checkpoint_file",
    "checkpoint_certificate", "freeze_plan_sha256", "inspector_binary_sha256",
    "recovery_manifest_sha256", "release_commit", "release_tag", "repository",
    "schema_version", "verified_quorum",
}
assert descriptor["schema_version"] == "arc-recovery-checkpoint-descriptor/v1"
assert descriptor["repository"] == "FerrumVir/arc-chain"
assert descriptor["release_tag"] == "v0.8.0"
assert descriptor["release_commit"] == "9" * 40
assert descriptor["checkpoint_file"] == {
    "filename": "recovery.arcchkpt",
    "sha256": hashlib.sha256(b"ARCCHKPT deterministic release fixture v1\n").hexdigest(),
    "size_bytes": len(b"ARCCHKPT deterministic release fixture v1\n"),
}
identity = descriptor["canonical_inspection"]
assert set(identity) == {
    "chain_id", "community_rewards_v1_activation_height", "created_at_unix_ms",
    "format_version", "full_state_root", "manifest_hash", "network_genesis_hash",
    "payload_hash", "protocol_version", "recovery_domain", "recovery_epoch",
    "source_block_hash", "source_consensus_round", "source_height",
    "source_state_root", "transition_block_hash", "transition_height",
    "validator_count", "validator_set_id",
}
assert identity["format_version"] == 1
assert identity["chain_id"] == "0x415243"
assert re.fullmatch(r"[0-9a-f]{64}", identity["payload_hash"])
assert identity["community_rewards_v1_activation_height"] == 137146
assert identity["source_height"] == 137145
assert identity["transition_height"] == 137146
assert identity["recovery_epoch"] == 1
assert identity["validator_set_id"] == 1
assert identity["validator_count"] == 6
assert identity["protocol_version"] == "3.0.0"
certificate = descriptor["checkpoint_certificate"]
assert set(certificate) == {"signatures", "signing_hash", "validators"}
assert re.fullmatch(r"[0-9a-f]{64}", certificate["signing_hash"])
assert len(certificate["validators"]) == 6
assert len(certificate["signatures"]) == 5
assert [row["address"] for row in certificate["validators"]] == sorted(
    row["address"] for row in certificate["validators"]
)
assert {row["address"]: row["stake"] for row in certificate["validators"]} == {
    row["address"]: row["stake"] for row in descriptor["approved_validators"]
}
assert all(re.fullmatch(r"[0-9a-f]{64}", row["public_key"]) for row in certificate["validators"])
assert all(re.fullmatch(r"[0-9a-f]{128}", row["signature"]) for row in certificate["signatures"])
signed_addresses = [row["validator"] for row in certificate["signatures"]]
signed_stake = sum(
    row["stake"] for row in certificate["validators"] if row["address"] in signed_addresses
)
total_stake = sum(row["stake"] for row in certificate["validators"])
assert descriptor["verified_quorum"] == {
    "required_signatures": 5,
    "signed_stake": signed_stake,
    "signed_validator_addresses": signed_addresses,
    "status": "VERIFIED_QUORUM",
    "total_stake": total_stake,
    "validator_count": 6,
    "verified_signature_count": 5,
}
assert signed_stake * 3 > total_stake * 2
assert policy["schema_version"] == "arc-cutover-policy/v1"
assert policy["repository"] == "FerrumVir/arc-chain"
assert policy["release_tag"] == "v0.8.0"
assert policy["release_commit"] == "9" * 40
assert policy["recovery_checkpoint_descriptor_sha256"] == hashlib.sha256(descriptor_raw).hexdigest()
assert policy["recovery_checkpoint_file_sha256"] == descriptor["checkpoint_file"]["sha256"]
assert policy["canonical_boundary_height"] == identity["source_height"] == 137145
assert policy["required_post_cutover_min_height"] == identity["transition_height"] == 137146
assert policy["required_recovery_epoch"] == identity["recovery_epoch"] == 1
assert policy["required_validator_set_id"] == identity["validator_set_id"] == 1
assert policy["required_validator_count"] == identity["validator_count"] == 6
assert policy["checkpoint_format_version"] == identity["format_version"] == 1
assert policy["chain_id"] == identity["chain_id"]
assert policy["payload_hash"] == identity["payload_hash"]
assert policy["community_rewards_v1_activation_height"] == identity["community_rewards_v1_activation_height"] == 137146
assert policy["protocol_version"] == identity["protocol_version"]
for field in ("network_genesis_hash", "source_block_hash", "source_state_root", "transition_block_hash", "full_state_root", "recovery_domain"):
    assert policy[field] == identity[field]
assert policy["checkpoint_manifest_hash"] == identity["manifest_hash"]
assert policy["checkpoint_source_consensus_round"] == identity["source_consensus_round"]
assert policy["checkpoint_created_at_unix_ms"] == identity["created_at_unix_ms"]
assert policy["uncompleted_job_disposition"] == "expired_noncanonical_at_cutover"
assert policy["legacy_exit_clean_claimed"] is False
assert policy["legacy_restart_allowed"] is False
assert policy["global_legacy_absence_claimed"] is False
assert policy["offline_retirement_receipt_required"] is True
assert policy["v08_start_requires_offline_receipt"] is True
assert policy["legacy_admission_cutoff_utc"] == policy["all_controlled_stopped_at"]
assert policy["freeze_plan_sha256"] == descriptor["freeze_plan_sha256"]
assert policy["capture_id"] == descriptor["capture_id"]
assert policy["recovery_manifest_sha256"] == descriptor["recovery_manifest_sha256"]
assert policy["legacy_worker_rpc"] == {
    "claim_path": "/community/claim_work",
    "listener_ports": [9090, 3001],
    "submit_path": "/community/submit_work",
}
assert policy["legacy_validators"] == descriptor["approved_validators"]
assert len(policy["legacy_validators"]) == 6
assert [(row["name"], row["host"], row["origin"]) for row in policy["legacy_validators"]] == [
    ("nyc", "149.28.32.76", "http://149.28.32.76:9090"),
    ("lax", "140.82.16.112", "http://140.82.16.112:9090"),
    ("ams", "136.244.109.1", "http://136.244.109.1:9090"),
    ("lhr", "104.238.171.11", "http://104.238.171.11:9090"),
    ("nrt", "202.182.107.41", "http://202.182.107.41:9090"),
    ("sgp", "149.28.153.31", "http://149.28.153.31:9090"),
]
assert [(row["address"], row["stake"]) for row in policy["legacy_validators"]] == [
    ("adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f", 6666667),
    ("44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780", 6666667),
    ("5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21", 6666667),
    ("228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd", 6666667),
    ("f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f", 6666666),
    ("f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b", 6666666),
]
assert policy["checkpoint_quorum"] == descriptor["verified_quorum"]
PY
}

complete_scheduled_genesis_is_preserved() {
    local sandbox genesis_file
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    genesis_file="$sandbox/complete-scheduled-genesis.toml"
    cat >"$genesis_file" <<'TOML'
[chain]
name = "arc-release-production-fixture"
chain_id = "0x415243"
validator_set_complete = true
community_rewards_v1_activation_height = 10_000

[[accounts]]
address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
balance = 0

[[validators]]
address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
stake = 5_000_000
TOML
    if ! run_assembler "$sandbox" 'v0.8.0' "$genesis_file" \
        >"$sandbox/complete-scheduled.out" 2>&1; then
        sed -n '1,120p' "$sandbox/complete-scheduled.out"
        return 1
    fi
    assert_file_contains "$sandbox/output/genesis.toml" \
        '^community_rewards_v1_activation_height[[:space:]]*=[[:space:]]*10_000$' \
        'release did not preserve the approved explicit activation schedule' || return 1
    assert_file_contains "$sandbox/output/SHA256SUMS" '[[:space:]]genesis\.toml$' \
        'scheduled production genesis is not protected by the checksum manifest' || return 1
}

unsafe_production_genesis_is_rejected_before_manifest() {
    local sandbox genesis_file output status
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    genesis_file="$sandbox/unsafe-genesis.toml"
    cat >"$genesis_file" <<'TOML'
[chain]
name = "unsafe-production-fixture"
chain_id = "0x415243"
validator_set_complete = true

[[validators]]
insecure_dev_seed = "release-contract-must-reject-this"
stake = 5_000_000
TOML
    output="$sandbox/unsafe-genesis.out"
    run_assembler "$sandbox" 'v0.8.0' "$genesis_file" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler accepted a complete genesis containing deterministic identity material\n'
        return 1
    fi
    assert_file_contains "$output" 'forbidden secret-bearing field.*insecure_dev_seed' \
        'unsafe-genesis refusal did not identify the forbidden field' || return 1
    if [ -s "$sandbox/output/SHA256SUMS" ]; then
        printf 'unsafe genesis left a publish-ready checksum manifest\n'
        return 1
    fi
}

every_headless_asset_is_individually_required() {
    local asset sandbox output status
    for asset in $CANONICAL_HEADLESS_ASSETS; do
        new_assembly_fixture
        sandbox="$NEW_ASSEMBLY_SANDBOX"
        output="$sandbox/missing.out"
        find "$sandbox/artifacts" -type f -name "$asset" -delete
        run_assembler "$sandbox" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'assembler accepted release with missing canonical asset: %s\n' "$asset"
            return 1
        fi
        if ! grep -Fq "$asset" "$output"; then
            printf 'missing-asset failure did not identify %s:\n' "$asset"
            sed -n '1,80p' "$output"
            return 1
        fi
        if [ -s "$sandbox/output/SHA256SUMS" ]; then
            printf 'failed assembly left a publish-ready SHA256SUMS after %s was missing\n' "$asset"
            return 1
        fi
    done
}

duplicate_asset_is_rejected() {
    local sandbox output status
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    write_nonempty "$sandbox/artifacts/duplicate/arc-node-linux-x86_64" 'collision fixture'
    output="$sandbox/duplicate.out"
    run_assembler "$sandbox" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler silently chose one of two same-named assets\n'
        return 1
    fi
    grep -Eq 'exactly one.*arc-node-linux-x86_64.*found 2' "$output" || {
        printf 'duplicate failure was not explicit:\n'
        sed -n '1,80p' "$output"
        return 1
    }
}

cutover_handoff_is_mandatory_and_hash_bound() {
    local sandbox output status
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    chmod 600 "$sandbox/cutover-handoff/arc-legacy-maintenance-boundary.json"
    printf '\n' >> "$sandbox/cutover-handoff/arc-legacy-maintenance-boundary.json"
    chmod 444 "$sandbox/cutover-handoff/arc-legacy-maintenance-boundary.json"
    output="$sandbox/cutover-tamper.out"
    run_assembler "$sandbox" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler accepted a modified maintenance boundary\n'
        return 1
    fi
    assert_file_contains "$output" \
        'legacy maintenance boundary must be one canonical JSON object|provenance/hash/time binding differs' \
        'cutover handoff tamper was not rejected at its semantic/hash boundary' || return 1

    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    mv "$sandbox/cutover-handoff/arc-recovery-checkpoint-descriptor.json" \
        "$sandbox/cutover-handoff/arc-recovery-checkpoint-descriptor.json.missing"
    output="$sandbox/cutover-missing.out"
    run_assembler "$sandbox" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler accepted an incomplete protected cutover handoff\n'
        return 1
    fi
    assert_file_contains "$output" 'membership differs from the exact three-file contract' \
        'missing cutover artifact was not rejected by the exact membership gate' || return 1
}

cutover_fixture_assembles_deterministically() {
    local first second first_hash second_hash
    new_assembly_fixture
    first="$NEW_ASSEMBLY_SANDBOX"
    run_assembler "$first" >"$first/deterministic.out" 2>&1 || return 1
    new_assembly_fixture
    second="$NEW_ASSEMBLY_SANDBOX"
    run_assembler "$second" >"$second/deterministic.out" 2>&1 || return 1
    for filename in \
        arc-legacy-maintenance-boundary.json \
        arc-recovery-checkpoint-descriptor.json \
        arc-cutover-policy.json; do
        first_hash="$(shasum -a 256 "$first/output/$filename" | awk '{print $1}')"
        second_hash="$(shasum -a 256 "$second/output/$filename" | awk '{print $1}')"
        assert_equals "$first_hash" "$second_hash" \
            "cutover fixture asset is not deterministic: $filename" || return 1
    done
}

non_semver_release_tag_is_rejected() {
    local sandbox output status
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    output="$sandbox/tag.out"
    run_assembler "$sandbox" 'v0.8.0-rc1' >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler accepted non-strict release tag v0.8.0-rc1\n'
        return 1
    fi
}

assembler_preserves_unowned_and_last_good_outputs() {
    local sandbox before_hash after_hash
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    printf 'do not delete\n' > "$sandbox/output/important.txt"
    if run_assembler "$sandbox" >"$sandbox/unowned.out" 2>&1; then
        printf 'assembler replaced an unowned non-empty output directory\n'
        return 1
    fi
    [ "$(cat "$sandbox/output/important.txt")" = 'do not delete' ] || {
        printf 'assembler altered an unowned output directory on refusal\n'
        return 1
    }

    rm -- "$sandbox/output/important.txt"
    run_assembler "$sandbox" >"$sandbox/first.out" 2>&1 || return 1
    before_hash="$(shasum -a 256 "$sandbox/output/SHA256SUMS")"
    find "$sandbox/artifacts" -type f -name arc-node-linux-x86_64 -delete
    if run_assembler "$sandbox" >"$sandbox/failed-rebuild.out" 2>&1; then
        printf 'assembler unexpectedly accepted a missing required artifact\n'
        return 1
    fi
    after_hash="$(shasum -a 256 "$sandbox/output/SHA256SUMS")"
    [ "$before_hash" = "$after_hash" ] || {
        printf 'failed release assembly replaced the last good output\n'
        return 1
    }
}

run_test 'complete fixture produces exact-tag manifest and verified SHA256SUMS' complete_fixture_produces_verifiable_contract
run_test 'complete production genesis preserves its explicit activation schedule' complete_scheduled_genesis_is_preserved
run_test 'unsafe production genesis is rejected before a publishable manifest exists' unsafe_production_genesis_is_rejected_before_manifest
run_test 'each of the ten canonical headless assets is independently required' every_headless_asset_is_individually_required
run_test 'duplicate same-named artifacts fail closed' duplicate_asset_is_rejected
run_test 'protected cutover handoff is mandatory, exact, and hash bound' cutover_handoff_is_mandatory_and_hash_bound
run_test 'cutover fixtures produce deterministic release assets' cutover_fixture_assembles_deterministically
run_test 'release assembly accepts only strict vX.Y.Z tags' non_semver_release_tag_is_rejected
run_test 'release assembly preserves unowned and last-good outputs' assembler_preserves_unowned_and_last_good_outputs

finish_tests

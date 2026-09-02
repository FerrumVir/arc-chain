#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

VALIDATOR="$REPO_ROOT/scripts/release/validate-genesis.py"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/arc-genesis-contract.XXXXXX")"
cleanup() { rm -rf -- "$SANDBOX"; }
trap cleanup EXIT

validate() {
    python3 "$VALIDATOR" "$1"
}

explicit_empty_observer_placeholder_is_accepted() {
    local fixture="$SANDBOX/observer.toml"
    cat >"$fixture" <<'TOML'
[chain]
name = "arc-release-observer-fixture"
chain_id = "0x415243"
validator_set_complete = false
TOML
    local output
    output="$(validate "$fixture")" || return 1
    printf '%s\n' "$output" | grep -Fq 'stake-zero community-observer placeholder' || return 1
    printf '%s\n' "$output" | grep -Fq 'community rewards v1 disabled (activation absent)'
}

complete_public_validator_set_is_accepted() {
    local fixture="$SANDBOX/complete.toml"
    cat >"$fixture" <<'TOML'
[chain]
name = "arc-release-production-fixture"
chain_id = "0x415243"
validator_set_complete = true

[[accounts]]
address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
balance = 100

[[validators]]
address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
stake = 5_000_000
TOML
    local output
    output="$(validate "$fixture")" || return 1
    printf '%s\n' "$output" | grep -Fq '1 public address(es) and no secret material' || return 1
    printf '%s\n' "$output" | grep -Fq 'community rewards v1 disabled (activation absent)'
}

explicit_reward_activation_is_accepted_only_for_complete_genesis() {
    local complete_fixture="$SANDBOX/complete-scheduled.toml"
    local observer_fixture="$SANDBOX/observer-scheduled.toml"
    local output status
    cat >"$complete_fixture" <<'TOML'
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
    validate "$complete_fixture" \
        | grep -Fq 'community rewards v1 activation height 10000 is explicit' \
        || return 1

    cat >"$observer_fixture" <<'TOML'
[chain]
name = "arc-release-observer-fixture"
chain_id = "0x415243"
validator_set_complete = false
community_rewards_v1_activation_height = 10_000
TOML
    output="$SANDBOX/observer-scheduled.out"
    validate "$observer_fixture" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'validator accepted reward activation on an incomplete observer genesis\n'
        return 1
    fi
    grep -Fq 'incomplete community-observer genesis must not schedule community reward activation' "$output"
}

reward_activation_must_be_a_representable_integer() {
    local value name fixture output status
    for name in boolean sentinel overflow; do
        case "$name" in
            boolean) value=true ;;
            sentinel) value=18446744073709551615 ;;
            overflow) value=18446744073709551616 ;;
        esac
        fixture="$SANDBOX/activation-$name.toml"
        {
            printf '%s\n' \
                '[chain]' \
                'name = "arc-release-production-fixture"' \
                'chain_id = "0x415243"' \
                'validator_set_complete = true'
            printf 'community_rewards_v1_activation_height = %s\n' "$value"
            printf '%s\n' \
                '' \
                '[[validators]]' \
                'address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' \
                'stake = 5_000_000'
        } >"$fixture"
        output="$SANDBOX/activation-$name.out"
        validate "$fixture" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'validator accepted invalid reward activation: %s\n' "$name"
            return 1
        fi
        grep -Fq 'community_rewards_v1_activation_height' "$output" || {
            printf 'activation rejection did not name the field for %s:\n' "$name"
            cat "$output"
            return 1
        }
    done
}

shipped_genesis_templates_are_identical_recovered_network() {
    local canonical="$REPO_ROOT/genesis.toml" template output actual_sha
    local expected_sha=8394894aaf32aff64df5c6988186e4802cb77a62daf259d8f5cab11d818ed269

    actual_sha="$(python3 - "$canonical" <<'PY'
import hashlib
import pathlib
import sys

print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" || return 1
    [ "$actual_sha" = "$expected_sha" ] || {
        printf 'canonical genesis does not match the approved recovery genesis: %s\n' "$actual_sha"
        return 1
    }
    for template in \
        "$canonical" \
        "$REPO_ROOT/deploy/config/genesis.toml" \
        "$REPO_ROOT/desktop/src-tauri/resources/genesis.toml"
    do
        output="$(validate "$template")" || return 1
        printf '%s\n' "$output" \
            | grep -Fq 'complete production validator set contains 6 public address(es)' \
            || return 1
        printf '%s\n' "$output" \
            | grep -Fq 'community rewards v1 activation height 137146 is explicit' \
            || return 1
        cmp -s "$canonical" "$template" || {
            printf 'shipped genesis template differs from canonical genesis.toml: %s\n' "$template"
            return 1
        }
    done
}

secret_bearing_identity_fields_are_rejected() {
    local field fixture output status
    for field in seed insecure_dev_seed private_key secret_key mnemonic; do
        fixture="$SANDBOX/forbidden-$field.toml"
        {
            printf '%s\n' \
                '[chain]' \
                'name = "unsafe-release-fixture"' \
                'chain_id = "0x415243"' \
                'validator_set_complete = true' \
                '' \
                '[[validators]]'
            printf '%s = "contract-fixture-value"\n' "$field"
            printf '%s\n' 'stake = 5_000_000'
        } >"$fixture"
        output="$SANDBOX/forbidden-$field.out"
        validate "$fixture" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'validator accepted forbidden identity field: %s\n' "$field"
            return 1
        fi
        grep -Fq "forbidden secret-bearing field at genesis.validators[1].$field" "$output" || {
            printf 'validator did not identify forbidden field %s:\n' "$field"
            cat "$output"
            return 1
        }
    done
}

embedded_private_key_material_is_rejected() {
    local fixture="$SANDBOX/private-marker.toml" output="$SANDBOX/private-marker.out"
    local marker status
    marker='-----BEGIN PRIVATE'' KEY-----'
    {
        printf '%s\n' '[chain]'
        printf 'name = "%s"\n' "$marker"
        printf '%s\n' \
            'chain_id = "0x415243"' \
            'validator_set_complete = false'
    } >"$fixture"
    validate "$fixture" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'validator accepted embedded private-key material\n'
        return 1
    fi
    grep -Fq 'private-key material is forbidden' "$output"
}

incomplete_genesis_cannot_ship_a_partial_validator_set() {
    local fixture="$SANDBOX/partial.toml" output="$SANDBOX/partial.out" status
    cat >"$fixture" <<'TOML'
[chain]
name = "partial-release-fixture"
chain_id = "0x415243"
validator_set_complete = false

[[validators]]
address = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
stake = 5_000_000
TOML
    validate "$fixture" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'validator accepted an incomplete partial validator set\n'
        return 1
    fi
    grep -Fq 'must not contain a partial validator list' "$output"
}

validator_mode_must_be_explicit() {
    local fixture="$SANDBOX/implicit.toml" output="$SANDBOX/implicit.out" status
    cat >"$fixture" <<'TOML'
[chain]
name = "implicit-release-fixture"
chain_id = "0x415243"
TOML
    validate "$fixture" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'validator accepted an implicit validator-set mode\n'
        return 1
    fi
    grep -Fq 'missing required field(s): validator_set_complete' "$output"
}

validator_accounts_must_be_shared_genesis_state() {
    local fixture="$SANDBOX/missing-validator-account.toml"
    local output="$SANDBOX/missing-validator-account.out" status
    cat >"$fixture" <<'TOML'
[chain]
name = "missing-validator-account"
chain_id = "0x415243"
validator_set_complete = true

[[accounts]]
address = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
balance = 100

[[validators]]
address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
stake = 5_000_000
TOML
    validate "$fixture" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'validator accepted a production validator absent from genesis.accounts\n'
        return 1
    fi
    grep -Fq 'must also be declared in genesis.accounts' "$output" || return 1

    if grep -Eq \
        'accounts[.]push[(][(]validator_address|Adding validator .* genesis' \
        "$REPO_ROOT/crates/arc-node/src/main.rs"; then
        printf 'node startup still mutates genesis accounts from its local validator identity\n'
        return 1
    fi
}

run_test 'explicit empty genesis remains valid for stake-zero community observers' explicit_empty_observer_placeholder_is_accepted
run_test 'complete genesis accepts only public validator addresses and positive stake' complete_public_validator_set_is_accepted
run_test 'reward activation is explicit and forbidden on incomplete observer genesis' explicit_reward_activation_is_accepted_only_for_complete_genesis
run_test 'reward activation rejects booleans and the reserved/out-of-range u64 values' reward_activation_must_be_a_representable_integer
run_test 'seed, insecure-dev, private, secret, and mnemonic fields fail closed' secret_bearing_identity_fields_are_rejected
run_test 'embedded private-key material fails closed without being echoed' embedded_private_key_material_is_rejected
run_test 'incomplete genesis cannot publish a partial validator set' incomplete_genesis_cannot_ship_a_partial_validator_set
run_test 'release genesis must explicitly select complete or observer mode' validator_mode_must_be_explicit
run_test 'validator identities must be declared in shared genesis accounts' validator_accounts_must_be_shared_genesis_state
run_test 'all shipped genesis templates equal the approved recovered network' shipped_genesis_templates_are_identical_recovered_network

finish_tests

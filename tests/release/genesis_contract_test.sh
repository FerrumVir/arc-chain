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
    validate "$fixture" | grep -Fq 'stake-zero community-observer placeholder'
}

complete_public_validator_set_is_accepted() {
    local fixture="$SANDBOX/complete.toml"
    cat >"$fixture" <<'TOML'
[chain]
name = "arc-release-production-fixture"
chain_id = "0x415243"
validator_set_complete = true

[[accounts]]
address = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
balance = 100

[[validators]]
address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
stake = 5_000_000
TOML
    validate "$fixture" | grep -Fq '1 public address(es) and no secret material'
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

run_test 'explicit empty genesis remains valid for stake-zero community observers' explicit_empty_observer_placeholder_is_accepted
run_test 'complete genesis accepts only public validator addresses and positive stake' complete_public_validator_set_is_accepted
run_test 'seed, insecure-dev, private, secret, and mnemonic fields fail closed' secret_bearing_identity_fields_are_rejected
run_test 'embedded private-key material fails closed without being echoed' embedded_private_key_material_is_rejected
run_test 'incomplete genesis cannot publish a partial validator set' incomplete_genesis_cannot_ship_a_partial_validator_set
run_test 'release genesis must explicitly select complete or observer mode' validator_mode_must_be_explicit

finish_tests

#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

ACTIVE_FIXTURES=""
NEW_FIXTURE=""
cleanup_fixtures() {
    local fixture
    for fixture in $ACTIVE_FIXTURES; do
        [ -d "$fixture" ] || continue
        find "$fixture" -type f -delete
        rmdir "$fixture"
    done
}
trap cleanup_fixtures EXIT

new_fixture() {
    local fixture
    fixture="$(mktemp -d "${TMPDIR:-/tmp}/arc-release-signature.XXXXXX")"
    ACTIVE_FIXTURES="$ACTIVE_FIXTURES $fixture"
    ssh-keygen -q -t ed25519 -N '' -f "$fixture/key"
    ssh-keygen -q -t ed25519 -N '' -f "$fixture/wrong-key"
    printf '%s\n' \
        '# ARC release manifest v1' \
        '# repository=FerrumVir/arc-chain' \
        '# tag=v0.8.0' \
        '# commit=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
        'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  arc-node-linux-x86_64' \
        > "$fixture/SHA256SUMS"
    printf 'arc-release namespaces="arc-release-manifest-v1" %s\n' \
        "$(cat "$fixture/key.pub")" > "$fixture/allowed"
    printf 'arc-release namespaces="arc-release-manifest-v1" %s\n' \
        "$(cat "$fixture/wrong-key.pub")" > "$fixture/wrong-allowed"
    ssh-keygen -Y sign \
        -f "$fixture/key" \
        -n arc-release-manifest-v1 \
        "$fixture/SHA256SUMS" >/dev/null
    NEW_FIXTURE="$fixture"
}

correct_signature_passes() {
    local fixture
    new_fixture
    fixture="$NEW_FIXTURE"
    ssh-keygen -Y verify \
        -f "$fixture/allowed" \
        -I arc-release \
        -n arc-release-manifest-v1 \
        -s "$fixture/SHA256SUMS.sig" \
        < "$fixture/SHA256SUMS" >/dev/null
}

manifest_tamper_is_rejected() {
    local fixture
    new_fixture
    fixture="$NEW_FIXTURE"
    printf '%s\n' '# tampered=true' >> "$fixture/SHA256SUMS"
    ! ssh-keygen -Y verify \
        -f "$fixture/allowed" \
        -I arc-release \
        -n arc-release-manifest-v1 \
        -s "$fixture/SHA256SUMS.sig" \
        < "$fixture/SHA256SUMS" >/dev/null 2>&1
}

wrong_key_namespace_and_principal_are_rejected() {
    local fixture
    new_fixture
    fixture="$NEW_FIXTURE"
    ! ssh-keygen -Y verify \
        -f "$fixture/wrong-allowed" \
        -I arc-release \
        -n arc-release-manifest-v1 \
        -s "$fixture/SHA256SUMS.sig" \
        < "$fixture/SHA256SUMS" >/dev/null 2>&1 || return 1
    ! ssh-keygen -Y verify \
        -f "$fixture/allowed" \
        -I arc-release \
        -n wrong-namespace \
        -s "$fixture/SHA256SUMS.sig" \
        < "$fixture/SHA256SUMS" >/dev/null 2>&1 || return 1
    ! ssh-keygen -Y verify \
        -f "$fixture/allowed" \
        -I wrong-principal \
        -n arc-release-manifest-v1 \
        -s "$fixture/SHA256SUMS.sig" \
        < "$fixture/SHA256SUMS" >/dev/null 2>&1
}

run_test 'correct namespaced owner-style manifest signature verifies' correct_signature_passes
run_test 'one-byte-equivalent manifest mutation rejects the signature' manifest_tamper_is_rejected
run_test 'wrong key, namespace, and principal all fail closed' wrong_key_namespace_and_principal_are_rejected
finish_tests

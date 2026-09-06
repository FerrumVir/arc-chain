#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

BACKUP_SCRIPT="$REPO_ROOT/scripts/release/backup-signing-keys.sh"
VERIFY_SCRIPT="$REPO_ROOT/scripts/release/verify-signing-key-backup.sh"
ACTIVE_FIXTURES=""
NEW_FIXTURE=""
PASSPHRASE='arc-signing-backup-behavior-sentinel-2026-08-28'

cleanup_fixtures() {
    local fixture
    for fixture in $ACTIVE_FIXTURES; do
        [ -d "$fixture" ] || continue
        chmod -R u+rwX -- "$fixture" 2>/dev/null || true
        find "$fixture" -depth -delete
    done
}
trap cleanup_fixtures EXIT

new_fixture() {
    local fixture
    fixture="$(mktemp -d "${TMPDIR:-/tmp}/arc-signing-backup-test.XXXXXX")"
    ACTIVE_FIXTURES="$ACTIVE_FIXTURES $fixture"
    mkdir -m 700 -- "$fixture/tmp"
    ssh-keygen -q -t ed25519 -N '' -f "$fixture/manifest-key"
    printf '%s\n' 'untrusted comment: test-only Tauri private key' \
        'RWRCSGV5VGhhdElzTm90QVJlYWxQcml2YXRlS2V5' > "$fixture/tauri-key"
    chmod 600 "$fixture/manifest-key" "$fixture/tauri-key"
    NEW_FIXTURE="$fixture"
}

canonicalize_with_verifier() {
    local input_path="$1"
    local output_path="$2"
    local function_source
    function_source="$(sed -n \
        '/^canonicalize_manifest_public_key() {$/,/^}$/p' \
        "$VERIFY_SCRIPT")"
    [ -n "$function_source" ] || {
        printf 'manifest public-key canonicalizer is unavailable\n'
        return 1
    }
    {
        printf '%s\n' "$function_source"
        printf '%s\n' 'canonicalize_manifest_public_key "$1" "$2"'
    } | /bin/bash -s -- "$input_path" "$output_path"
}

manifest_public_key_comments_do_not_change_identity() {
    local fixture commented_key no_comment_key commented_raw uncommented_raw
    local no_comment_raw unexpected_comment_raw commented_canonical
    local uncommented_canonical no_comment_canonical
    new_fixture
    fixture="$NEW_FIXTURE"
    commented_key="$fixture/commented-manifest-key"
    no_comment_key="$fixture/no-comment-manifest-key"
    commented_raw="$fixture/commented.pub"
    uncommented_raw="$fixture/uncommented.pub"
    no_comment_raw="$fixture/no-comment.pub"
    unexpected_comment_raw="$fixture/unexpected-comment.pub"
    commented_canonical="$fixture/commented.canonical"
    uncommented_canonical="$fixture/uncommented.canonical"
    no_comment_canonical="$fixture/no-comment.canonical"

    ssh-keygen -q -t ed25519 -N '' \
        -C 'arc-release-manifest-v1' -f "$commented_key"
    ssh-keygen -q -t ed25519 -N '' -C '' -f "$no_comment_key"
    ssh-keygen -y -f "$commented_key" > "$commented_raw"
    awk '{print $1 " " $2}' "$commented_raw" > "$uncommented_raw"
    awk '{print $1 " " $2 " unexpected-comment"}' "$commented_raw" \
        > "$unexpected_comment_raw"
    ssh-keygen -y -f "$no_comment_key" > "$no_comment_raw"

    canonicalize_with_verifier "$commented_raw" "$commented_canonical" \
        || return 1
    canonicalize_with_verifier "$uncommented_raw" "$uncommented_canonical" \
        || return 1
    canonicalize_with_verifier "$no_comment_raw" "$no_comment_canonical" \
        || return 1
    cmp -s "$commented_canonical" "$uncommented_canonical" || {
        printf 'public-key comment changed the canonical key identity\n'
        return 1
    }
    if cmp -s "$commented_canonical" "$no_comment_canonical"; then
        printf 'different manifest keys collapsed to one canonical identity\n'
        return 1
    fi
    if canonicalize_with_verifier \
        "$unexpected_comment_raw" "$fixture/unexpected-comment.canonical"; then
        printf 'unexpected manifest key comment passed strict canonicalization\n'
        return 1
    fi
}

encrypted_backup_round_trips_and_cleans_plaintext() {
    local fixture output restored members expected leftovers
    new_fixture
    fixture="$NEW_FIXTURE"
    output="$fixture/arc-signing-keys.tar.gpg"

    TMPDIR="$fixture/tmp" ARC_SIGNING_BACKUP_PASSPHRASE="$PASSPHRASE" \
        "$BACKUP_SCRIPT" "$fixture/tauri-key" "$fixture/manifest-key" "$output" \
        >/dev/null || return 1
    [ -s "$output" ] && [ ! -L "$output" ] || return 1
    leftovers="$(find "$fixture/tmp" -mindepth 1 -maxdepth 1 \
        -name 'arc-signing-backup.*' -print -quit)"
    [ -z "$leftovers" ] || {
        printf 'plaintext work directory survived successful backup: %s\n' "$leftovers"
        return 1
    }

    restored="$fixture/restored.tar"
    printf '%s' "$PASSPHRASE" | gpg --batch --yes --no-symkey-cache \
        --pinentry-mode loopback --passphrase-fd 0 --decrypt \
        --output "$restored" "$output" >/dev/null 2>&1 || return 1
    members="$(tar -tf "$restored" | LC_ALL=C sort)"
    expected="$(printf '%s\n' KEY-SHA256SUMS release-manifest-ed25519 \
        release-manifest-ed25519.pub tauri-updater.key | LC_ALL=C sort)"
    [ "$members" = "$expected" ] || {
        printf 'unexpected restored archive members:\n%s\n' "$members"
        return 1
    }
    mkdir -m 700 -- "$fixture/restored"
    tar -xf "$restored" -C "$fixture/restored"
    (cd "$fixture/restored" && shasum -a 256 -c KEY-SHA256SUMS >/dev/null)
}

wrong_passphrase_and_truncation_fail_closed() {
    local fixture output size
    new_fixture
    fixture="$NEW_FIXTURE"
    output="$fixture/arc-signing-keys.tar.gpg"
    TMPDIR="$fixture/tmp" ARC_SIGNING_BACKUP_PASSPHRASE="$PASSPHRASE" \
        "$BACKUP_SCRIPT" "$fixture/tauri-key" "$fixture/manifest-key" "$output" \
        >/dev/null || return 1

    if printf '%s' 'definitely-wrong-backup-passphrase-0000000000' | gpg \
        --batch --yes --no-symkey-cache --pinentry-mode loopback \
        --passphrase-fd 0 --decrypt --output "$fixture/wrong.tar" "$output" \
        >/dev/null 2>&1; then
        printf 'wrong passphrase decrypted the backup\n'
        return 1
    fi

    cp -- "$output" "$fixture/truncated.tar.gpg"
    size="$(wc -c < "$fixture/truncated.tar.gpg" | tr -d ' ')"
    truncate -s "$((size - 1))" "$fixture/truncated.tar.gpg"
    if printf '%s' "$PASSPHRASE" | gpg --batch --yes --no-symkey-cache \
        --pinentry-mode loopback --passphrase-fd 0 --decrypt \
        --output "$fixture/truncated.tar" "$fixture/truncated.tar.gpg" \
        >/dev/null 2>&1; then
        printf 'truncated ciphertext decrypted successfully\n'
        return 1
    fi
}

existing_output_is_never_replaced() {
    local fixture output before after
    new_fixture
    fixture="$NEW_FIXTURE"
    output="$fixture/arc-signing-keys.tar.gpg"
    TMPDIR="$fixture/tmp" ARC_SIGNING_BACKUP_PASSPHRASE="$PASSPHRASE" \
        "$BACKUP_SCRIPT" "$fixture/tauri-key" "$fixture/manifest-key" "$output" \
        >/dev/null || return 1
    before="$(shasum -a 256 "$output" | awk '{print $1}')"
    if TMPDIR="$fixture/tmp" ARC_SIGNING_BACKUP_PASSPHRASE="$PASSPHRASE" \
        "$BACKUP_SCRIPT" "$fixture/tauri-key" "$fixture/manifest-key" "$output" \
        >/dev/null 2>&1; then
        printf 'backup script replaced an existing output\n'
        return 1
    fi
    after="$(shasum -a 256 "$output" | awk '{print $1}')"
    [ "$before" = "$after" ]
}

inherited_xtrace_never_discloses_passphrase() {
    local fixture log invalid
    new_fixture
    fixture="$NEW_FIXTURE"
    log="$fixture/xtrace.log"
    invalid="$fixture/invalid.tar.gpg"
    printf 'not ciphertext' > "$invalid"

    ARC_SIGNING_BACKUP_PASSPHRASE="$PASSPHRASE" /bin/bash -x \
        "$BACKUP_SCRIPT" "$fixture/missing-tauri" "$fixture/missing-manifest" \
        "$fixture/not-created.tar.gpg" >"$log" 2>&1 || true
    ARC_SIGNING_BACKUP_PASSPHRASE="$PASSPHRASE" /bin/bash -x \
        "$VERIFY_SCRIPT" "$invalid" aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
        bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
        >>"$log" 2>&1 || true
    if grep -Fq -- "$PASSPHRASE" "$log"; then
        printf 'inherited xtrace disclosed the backup passphrase\n'
        return 1
    fi
}

run_test 'encrypted signing-key backup round-trips and removes plaintext workdirs' \
    encrypted_backup_round_trips_and_cleans_plaintext
run_test 'manifest key comments are ignored without weakening key identity' \
    manifest_public_key_comments_do_not_change_identity
run_test 'wrong passphrase and truncated ciphertext fail closed' \
    wrong_passphrase_and_truncation_fail_closed
run_test 'existing ciphertext is create-only and never replaced' \
    existing_output_is_never_replaced
run_test 'inherited xtrace never discloses a signing backup passphrase' \
    inherited_xtrace_never_discloses_passphrase
finish_tests

#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

WORKFLOW="$REPO_ROOT/.github/workflows/validator-vault-rewrap.yml"
REWRAP="$REPO_ROOT/scripts/release/rewrap-validator-vault.sh"
VALIDATOR="$REPO_ROOT/scripts/release/validate-validator-vault.py"

one_shot_workflow_is_exact_main_protected_and_create_only() {
    [ -f "$WORKFLOW" ] || {
        printf 'one-shot validator-vault workflow is missing\n'
        return 1
    }
    if grep -Eq '^[[:space:]]+(push|pull_request|schedule):' "$WORKFLOW"; then
        printf 'validator-vault rewrap must remain manual-only\n'
        return 1
    fi
    for required in \
        'workflow_dispatch:' \
        'expected_main_sha:' \
        'expected_ciphertext_sha256:' \
        'expected_restore_cert_sha256:' \
        'confirmation:' \
        'environment: release' \
        'contents: read' \
        'persist-credentials: false' \
        '[ "$DISPATCH_REF" = refs/heads/main ]' \
        '[ "$EXPECTED_MAIN_SHA" = "$DISPATCH_SHA" ]' \
        'REWRAP ARC VALIDATOR VAULT $EXPECTED_MAIN_SHA' \
        'bdb2dd477fe10e06e63123d6080f321fce4a251479a5af8a59ae2b47814ed7e9' \
        '6707f8b1dbc1f2d37d9a873a7e3d2c870d2b46db36f15a6df5293547680bfd43' \
        'ARC_VALIDATOR_VAULT_CIPHERTEXT_B64' \
        'ARC_VALIDATOR_VAULT_PASSPHRASE' \
        'ARC_VALIDATOR_VAULT_RESTORE_CERT_B64' \
        'This protected job never checks out repository content' \
        'Inline-decrypt, canonicalize, and public-key rewrap' \
        '/usr/bin/python3 -I - "$plain_tar" "$canonical_tar"' \
        'canonical_tar="$work_dir/validator-vault.canonical.tar"' \
        '-in "$canonical_tar"' \
        'tarfile.USTAR_FORMAT' \
        'is_appledouble(payload)' \
        'basename == "PUBLIC-INVENTORY.json"' \
        'basename.endswith(".key")' \
        'public inventory parent mismatch' \
        'unexpected non-key vault member' \
        'archive.pax_headers' \
        'member.offset_data' \
        'os.O_EXCL' \
        'clear_secret_material' \
        'Reconfirm protected main after every secret and plaintext is gone' \
        'trap cleanup EXIT' \
        'retention-days: 1' \
        'compression-level: 0' \
        'overwrite: false' \
        'include-hidden-files: false' \
        'Remove runner copy of the encrypted restore artifact'
    do
        grep -Fq -- "$required" "$WORKFLOW" || {
            printf 'one-shot validator-vault workflow omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'contents:[[:space:]]+write|actions:[[:space:]]+write|id-token:[[:space:]]+write' \
        "$WORKFLOW"; then
        printf 'one-shot validator-vault workflow has an unnecessary write permission\n'
        return 1
    fi
    [ "$(grep -Fc 'uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a' \
        "$WORKFLOW")" -eq 1 ] || {
        printf 'one-shot validator-vault workflow must have one exact-SHA encrypted upload\n'
        return 1
    }
    rewrap_block="$(awk '
        /^  rewrap:/ { capture=1 }
        capture { print }
    ' "$WORKFLOW")"
    if printf '%s\n' "$rewrap_block" | grep -Fq 'actions/checkout@'; then
        printf 'protected validator-vault secret job checks out repository content\n'
        return 1
    fi
    secret_step="$(printf '%s\n' "$rewrap_block" | awk '
        /- id: rewrap/ { capture=1 }
        capture { print }
        capture && /- name: Reconfirm protected main after every secret/ { exit }
    ')"
    if printf '%s\n' "$secret_step" \
        | grep -Eq 'scripts\/|python3[[:space:]]+[^-]|(^|[[:space:]])(bash|sh|node|npm|cargo|rustc|git)[[:space:]]'; then
        printf 'validator-vault secret window can execute repository/package/compiler/Git code\n'
        return 1
    fi
    clear_line="$(printf '%s\n' "$secret_step" | grep -nF '          clear_secret_material' | tail -1 | cut -d: -f1)"
    receipt_line="$(printf '%s\n' "$secret_step" | grep -nF '          /usr/bin/jq -cS -n' | cut -d: -f1)"
    if [ -z "$clear_line" ] || [ -z "$receipt_line" ] || [ "$clear_line" -ge "$receipt_line" ]; then
        printf 'validator-vault plaintext is not cleared before post-secret receipt processing\n'
        return 1
    fi
    if printf '%s\n' "$secret_step" | grep -Fq '          /usr/bin/jq -n'; then
        printf 'validator-vault receipt can still be emitted as noncanonical pretty JSON\n'
        return 1
    fi
}

rewrap_uses_the_recovered_profile_and_safe_canonicalization() {
    for required in \
        'set +x' \
        'ulimit -c 0' \
        'trap cleanup EXIT' \
        'unset ARC_VALIDATOR_VAULT_PASSPHRASE' \
        '-aes-256-cbc' \
        '-pbkdf2' \
        '-iter 600000' \
        '-md sha256' \
        '-pass stdin' \
        'validate-validator-vault.py' \
        'openssl verify -purpose any' \
        '-checkend 86400' \
        'Key Encipherment|Data Encipherment|Key Agreement' \
        'E-mail Protection|Any Extended Key Usage' \
        'Public Key Algorithm: rsaEncryption' \
        'RSA_BITS' \
        '-aes-256-cbc' \
        'rsa_padding_mode:oaep' \
        'rsa_oaep_md:sha256' \
        'algorithm: rsaesOaep' \
        'algorithm: aes-256-gcm' \
        'ln -- "$CANDIDATE_CMS" "$OUTPUT"'
    do
        grep -Fq -- "$required" "$REWRAP" || {
            printf 'validator-vault rewrapper omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'tar[[:space:]]+(-t|--list|-x|--extract)|cat[[:space:]]+.*PLAIN|echo[^\n]*[$][{]?ARC_VALIDATOR_VAULT_PASSPHRASE' \
        "$REWRAP" "$WORKFLOW"; then
        printf 'validator-vault workflow can list/extract members or print secret material\n'
        return 1
    fi
    if grep -Fq 'archive.extractfile' "$WORKFLOW"; then
        printf 'protected validator-vault workflow reads members through tar extraction\n'
        return 1
    fi
    for required in \
        'mode="r:"' \
        'MAX_ARCHIVE_BYTES' \
        'MAX_MEMBERS' \
        'MAX_MEMBER_BYTES' \
        'MAX_TOTAL_FILE_BYTES' \
        'member.issym()' \
        'member.islnk()' \
        'member.issparse()' \
        'duplicates another archive path' \
        'private file permissions' \
        'fewer than six private vault files'
    do
        grep -Fq -- "$required" "$VALIDATOR" || {
            printf 'metadata-only safe-tar validator omits: %s\n' "$required"
            return 1
        }
    done
}

fixture_round_trip_is_cms_only_and_fail_closed() {
    local fixture passphrase cert_sha source_sha
    fixture="$(mktemp -d "${TMPDIR:-/tmp}/arc-validator-vault-test.XXXXXXXX")" || return 1
    passphrase='fixture-validator-vault-passphrase-DO-NOT-PRINT-0001'
    mkdir -p "$fixture/keys" "$fixture/output"
    for index in 1 2 3 4 5 6; do
        printf 'private validator fixture %s\n' "$index" > "$fixture/keys/validator-$index.key"
        chmod 600 "$fixture/keys/validator-$index.key"
    done
    COPYFILE_DISABLE=1 tar -cf "$fixture/vault.tar" -C "$fixture/keys" \
        validator-1.key validator-2.key validator-3.key \
        validator-4.key validator-5.key validator-6.key || return 1

    printf '%s' "$passphrase" | openssl enc \
        -aes-256-cbc -salt -pbkdf2 -iter 600000 -md sha256 \
        -pass stdin -in "$fixture/vault.tar" -out "$fixture/vault.tar.enc" \
        2>/dev/null || return 1
    openssl req -x509 -newkey rsa:3072 -nodes -days 2 \
        -subj '/CN=ARC validator vault contract fixture' \
        -addext 'basicConstraints=critical,CA:FALSE' \
        -addext 'keyUsage=critical,keyEncipherment,dataEncipherment' \
        -addext 'extendedKeyUsage=emailProtection' \
        -keyout "$fixture/restore.key.pem" \
        -out "$fixture/restore.cert.pem" >/dev/null 2>&1 || return 1

    source_sha="$(shasum -a 256 "$fixture/vault.tar.enc" | awk '{print $1}')"
    cert_sha="$(shasum -a 256 "$fixture/restore.cert.pem" | awk '{print $1}')"
    if ! RUNNER_TEMP="$fixture" ARC_VALIDATOR_VAULT_PASSPHRASE="$passphrase" \
        "$REWRAP" \
        "$fixture/vault.tar.enc" "$source_sha" \
        "$fixture/restore.cert.pem" "$cert_sha" \
        "$fixture/output/vault.tar.cms" \
        > "$fixture/rewrap.out" 2> "$fixture/rewrap.err"; then
        printf 'valid fixture did not rewrap\n'
        return 1
    fi
    [ ! -s "$fixture/rewrap.out" ] && [ ! -s "$fixture/rewrap.err" ] || {
        printf 'successful rewrap emitted output that could disclose vault metadata\n'
        return 1
    }
    openssl cms -decrypt -binary -inform DER \
        -recip "$fixture/restore.cert.pem" \
        -inkey "$fixture/restore.key.pem" \
        -in "$fixture/output/vault.tar.cms" \
        -out "$fixture/recovered.tar" 2>/dev/null || return 1
    cmp "$fixture/vault.tar" "$fixture/recovered.tar" || return 1

    if RUNNER_TEMP="$fixture" ARC_VALIDATOR_VAULT_PASSPHRASE="$passphrase" \
        "$REWRAP" \
        "$fixture/vault.tar.enc" "$(printf '0%.0s' {1..64})" \
        "$fixture/restore.cert.pem" "$cert_sha" \
        "$fixture/output/wrong-hash.tar.cms" \
        > "$fixture/failure.out" 2> "$fixture/failure.err"; then
        printf 'rewrapper accepted the wrong source ciphertext digest\n'
        return 1
    fi
    if grep -Fq -- "$passphrase" "$fixture/failure.out" "$fixture/failure.err"; then
        printf 'rewrapper disclosed the protected passphrase on failure\n'
        return 1
    fi

    chmod -R u+rwX "$fixture" 2>/dev/null || true
    rm -rf -- "$fixture"
}

python_archive_tests_pass() {
    python3 "$TEST_DIR/test_validator_vault_archive.py"
}

run_test 'one-shot vault rewrap is exact-main, protected, least-privilege, and create-only' \
    one_shot_workflow_is_exact_main_protected_and_create_only
run_test 'vault rewrap uses the recovered KDF and canonical safe-tar boundary' \
    rewrap_uses_the_recovered_profile_and_safe_canonicalization
run_test 'validator-vault safe-tar adversarial tests pass without member disclosure' \
    python_archive_tests_pass
run_test 'fixture vault round-trips only through the operator CMS key and fails closed' \
    fixture_round_trip_is_cms_only_and_fail_closed

finish_tests

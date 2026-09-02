#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

HELPER="$REPO_ROOT/scripts/release/restore-validator-vault.py"

restore_boundary_is_exact_commit_profile_and_private_extraction() {
    [ -x "$HELPER" ] || {
        printf 'validator-vault restore/install helper is missing or not executable\n'
        return 1
    }
    for required in \
        'arc.validator-vault-rewrap.v1' \
        'arc.pretag.artifact.v1' \
        'source_commit' \
        'RSA-OAEP-SHA256' \
        'AES-256-GCM' \
        'OpenSSLRuntime' \
        'DYLD_PRINT_LIBRARIES' \
        'LD_DEBUG' \
        'OPENSSL_CONF' \
        'did not load the reviewed private' \
        '--openssl-sha256' \
        '--openssl-libssl-sha256' \
        '--openssl-libcrypto-sha256' \
        'rsaesOaep' \
        'aes-256-gcm' \
        'OBJECT            :sha256' \
        'O_NOFOLLOW' \
        'O_EXCL' \
        'identity_before' \
        'write_all(descriptor, payload)' \
        'arc-validator-vault-restore.' \
        'pinned CMS restore inputs changed' \
        'pinned pre-tag ARC CLI changed' \
        'mode="r:"' \
        'archive.pax_headers' \
        'member.pax_headers' \
        'member.issparse()' \
        'duplicates another path' \
        'exactly six private keyfiles' \
        'keygen", "--verify-keyfile"' \
        'validator-public-keys.json' \
        'RESTORE-RECEIPT.json'
    do
        grep -Fq -- "$required" "$HELPER" || {
            printf 'validator-vault restore boundary omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq -- 'extractall|[.]extract[(]|shell[[:space:]]*=[[:space:]]*True|tar[[:space:]]+(-x|--extract)|print[(].*secret_key|REMOTE_SCRIPT.*secret_key' \
        "$HELPER"; then
        printf 'validator-vault restore can use unsafe extraction/shell execution or print private material\n'
        return 1
    fi
}

install_boundary_requires_offline_proof_strict_transport_and_no_clobber() {
    for required in \
        'arc.validator-vault.offline-stop-evidence.v2' \
        'arc.recovery.legacy-maintenance-evidence-bundle.v1' \
        'arc.recovery.legacy-maintenance-boundary.v1' \
        'legacy_maintenance_evidence_bundle_sha256' \
        'legacy_maintenance_boundary_sha256' \
        'arc.recovery.offline-stop-status.v1' \
        'arc.recovery.offline-stop.v4' \
        'stop_complete_sha256' \
        'stop_files_sha256' \
        'stopped_status_sha256' \
        'stopped_status_argv_sha256' \
        'fresh {node} stopped-status differs' \
        'validate_pinned_freeze_plan' \
        'ARC recovery capture v2\0' \
        '149.28.32.76' \
        '140.82.16.112' \
        '136.244.109.1' \
        '104.238.171.11' \
        '202.182.107.41' \
        '149.28.153.31' \
        '--offline-stop-evidence' \
        '--legacy-maintenance-evidence-bundle' \
        '--legacy-maintenance-evidence-bundle-sidecar' \
        '--legacy-maintenance-evidence-bundle-sha256' \
        '--legacy-maintenance-boundary' \
        '--legacy-maintenance-boundary-sidecar' \
        '--legacy-maintenance-boundary-sha256' \
        '--ssh-sha256' \
        '--scp-sha256' \
        '--ssh-identity' \
        '--ssh-identity-sha256' \
        'IdentityAgent=none' \
        'validate_exact_known_hosts' \
        'ssh-ed25519' \
        'pin_transport_runtime' \
        '"-S"' \
        'BatchMode=yes' \
        'StrictHostKeyChecking=yes' \
        'UserKnownHostsFile=' \
        'GlobalKnownHostsFile=/dev/null' \
        'HostKeyAlgorithms=ssh-ed25519' \
        'PubkeyAcceptedAlgorithms=ssh-ed25519' \
        'UpdateHostKeys=no' \
        'PasswordAuthentication=no' \
        'KbdInteractiveAuthentication=no' \
        'ForwardAgent=no' \
        'ClearAllForwardings=yes' \
        'IdentitiesOnly=yes' \
        'PreferredAuthentications=publickey' \
        '/etc/arc-v3/validator-key.json' \
        'ln -- "$temporary" "$destination"' \
        'chown root:root' \
        'chmod 0600' \
        'pinned {node} key changed during SCP upload' \
        'exact_mode=0o400' \
        'require_single_link=True' \
        'existing install receipt differs; replacement is forbidden'
    do
        grep -Fq -- "$required" "$HELPER" || {
            printf 'validator-vault install boundary omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq -- 'StrictHostKeyChecking=(no|accept-new)|sshpass|scp[(].*REMOTE_KEY_PATH|mv[[:space:]].*validator-key[.]json' \
        "$HELPER"; then
        printf 'validator-vault install retains a permissive transport or overwrite path\n'
        return 1
    fi
}

hermetic_restore_and_mock_transport_tests_pass() {
    python3 "$TEST_DIR/test_validator_vault_restore.py"
}

run_test 'validator-vault restore is exact-commit/profile bound and extracts only through private no-follow create-new paths' \
    restore_boundary_is_exact_commit_profile_and_private_extraction
run_test 'validator-key install requires authenticated fresh offline proof, strict pinned SSH, and create-only remote publication' \
    install_boundary_requires_offline_proof_strict_transport_and_no_clobber
run_test 'validator-vault restore/install adversarial and partial-resume tests pass' \
    hermetic_restore_and_mock_transport_tests_pass

finish_tests

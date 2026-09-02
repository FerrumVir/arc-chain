#!/usr/bin/env bash
# Rewrap one exact passphrase-encrypted validator vault to an operator's public
# CMS certificate. Plaintext exists only in a private temporary directory and
# is never listed, printed, extracted, or uploaded.
set +x
set -Eeuo pipefail
umask 077
ulimit -c 0
export LC_ALL=C
export LANG=C

die() {
    printf 'validator vault rewrap failed: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 5 ] || die \
    'usage: rewrap-validator-vault.sh SOURCE.enc EXPECTED_SOURCE_SHA256 RESTORE_CERT.pem EXPECTED_CERT_SHA256 OUTPUT.tar.cms'

SOURCE="$1"
EXPECTED_SOURCE_SHA256="$2"
RESTORE_CERT="$3"
EXPECTED_CERT_SHA256="$4"
OUTPUT="$5"
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

for command_name in awk chmod find grep ln mktemp openssl python3 rm rmdir sed; do
    command -v "$command_name" >/dev/null 2>&1 || die "required command is missing: $command_name"
done
if ! command -v sha256sum >/dev/null 2>&1 \
    && ! command -v shasum >/dev/null 2>&1; then
    die 'sha256sum or shasum is required'
fi
hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}
secure_delete() {
    local path="$1"
    if command -v shred >/dev/null 2>&1; then
        shred -u -z -n 1 -- "$path" >/dev/null 2>&1
    elif rm -P -- "$path" >/dev/null 2>&1; then
        :
    else
        rm -f -- "$path" >/dev/null 2>&1
    fi
}
[ -n "${ARC_VALIDATOR_VAULT_PASSPHRASE:-}" ] \
    || die 'protected validator-vault passphrase is unavailable'
[ "${#ARC_VALIDATOR_VAULT_PASSPHRASE}" -ge 32 ] \
    || die 'protected validator-vault passphrase is shorter than 32 characters'
[[ "$EXPECTED_SOURCE_SHA256" =~ ^[0-9a-f]{64}$ ]] \
    || die 'expected source digest is not a lowercase SHA-256'
[[ "$EXPECTED_CERT_SHA256" =~ ^[0-9a-f]{64}$ ]] \
    || die 'expected certificate digest is not a lowercase SHA-256'
[ -f "$SOURCE" ] && [ ! -L "$SOURCE" ] && [ -s "$SOURCE" ] \
    || die 'source ciphertext is missing, empty, or symlinked'
[ -f "$RESTORE_CERT" ] && [ ! -L "$RESTORE_CERT" ] && [ -s "$RESTORE_CERT" ] \
    || die 'restore certificate is missing, empty, or symlinked'
case "$OUTPUT" in
    /*.tar.cms) ;;
    *) die 'output must be an absolute .tar.cms path' ;;
esac
[ ! -e "$OUTPUT" ] && [ ! -L "$OUTPUT" ] \
    || die 'refusing to replace an existing CMS output'
[ -d "${OUTPUT%/*}" ] && [ ! -L "${OUTPUT%/*}" ] \
    || die 'CMS output parent must be a real directory'

[ "$(hash_file "$SOURCE")" = "$EXPECTED_SOURCE_SHA256" ] \
    || die 'source ciphertext SHA-256 differs from the authorized value'
[ "$(hash_file "$RESTORE_CERT")" = "$EXPECTED_CERT_SHA256" ] \
    || die 'restore certificate SHA-256 differs from the authorized value'
[ "$(grep -c '^-----BEGIN CERTIFICATE-----$' "$RESTORE_CERT" || true)" -eq 1 ] \
    || die 'restore certificate must contain exactly one PEM certificate'
[ "$(grep -c '^-----END CERTIFICATE-----$' "$RESTORE_CERT" || true)" -eq 1 ] \
    || die 'restore certificate must contain exactly one PEM certificate'
if grep -Eq -- '-----BEGIN .*PRIVATE KEY-----' "$RESTORE_CERT"; then
    die 'restore certificate secret unexpectedly contains private-key material'
fi

WORK_DIR="$(mktemp -d "${RUNNER_TEMP:-/tmp}/arc-validator-vault-rewrap.XXXXXXXX")"
PLAIN_TAR="$WORK_DIR/validator-vault.tar"
CANDIDATE_CMS="$WORK_DIR/validator-vault.tar.cms"
cleanup() {
    local status="$?"
    trap - EXIT
    unset ARC_VALIDATOR_VAULT_PASSPHRASE
    if [ -f "$PLAIN_TAR" ] && [ ! -L "$PLAIN_TAR" ]; then
        secure_delete "$PLAIN_TAR" || true
    fi
    if [ -d "$WORK_DIR" ] && [ ! -L "$WORK_DIR" ]; then
        find "$WORK_DIR" -xdev -type f -delete >/dev/null 2>&1 || true
        rmdir "$WORK_DIR" >/dev/null 2>&1 || true
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

CERT_TEXT="$WORK_DIR/certificate.txt"
KEY_USAGE="$WORK_DIR/key-usage.txt"
EXTENDED_USAGE="$WORK_DIR/extended-key-usage.txt"
CMS_TEXT="$WORK_DIR/cms.txt"

openssl x509 -in "$RESTORE_CERT" -noout -text > "$CERT_TEXT" 2>/dev/null \
    || die 'restore certificate is not valid X.509 PEM'
openssl verify -purpose any -CAfile "$RESTORE_CERT" "$RESTORE_CERT" \
    >/dev/null 2>&1 || die 'restore certificate is not currently valid and self-verifying'
openssl x509 -in "$RESTORE_CERT" -checkend 86400 -noout \
    >/dev/null 2>&1 || die 'restore certificate expires before the one-day artifact does'
grep -Fq 'Public Key Algorithm: rsaEncryption' "$CERT_TEXT" \
    || die 'restore certificate does not contain an RSA encryption key'
RSA_BITS="$(sed -n 's/.*Public-Key: (\([0-9][0-9]*\) bit).*/\1/p' "$CERT_TEXT")"
[[ "$RSA_BITS" =~ ^[0-9]+$ ]] && [ "$RSA_BITS" -ge 3072 ] \
    || die 'restore certificate RSA key is smaller than 3072 bits'

# An absent KeyUsage/EKU extension is unrestricted by X.509. When either is
# present, it must explicitly permit key transport/CMS use.
openssl x509 -in "$RESTORE_CERT" -noout -ext keyUsage \
    > "$KEY_USAGE" 2>/dev/null || die 'could not inspect certificate key usage'
if [ -s "$KEY_USAGE" ] \
    && ! grep -Eq 'Key Encipherment|Data Encipherment|Key Agreement' "$KEY_USAGE"; then
    die 'restore certificate key-usage extension does not permit encryption'
fi
openssl x509 -in "$RESTORE_CERT" -noout -ext extendedKeyUsage \
    > "$EXTENDED_USAGE" 2>/dev/null || die 'could not inspect certificate extended key usage'
if [ -s "$EXTENDED_USAGE" ] \
    && ! grep -Eq 'E-mail Protection|Any Extended Key Usage' "$EXTENDED_USAGE"; then
    die 'restore certificate extended-key-usage extension does not permit CMS use'
fi

# Creation profile recovered from the original vault operation. There is no
# fallback KDF/cipher guessing: one exact profile either authenticates the
# operator passphrase and yields the expected tar structure, or the run fails.
if ! printf '%s' "$ARC_VALIDATOR_VAULT_PASSPHRASE" | openssl enc -d \
    -aes-256-cbc \
    -pbkdf2 \
    -iter 600000 \
    -md sha256 \
    -pass stdin \
    -in "$SOURCE" \
    -out "$PLAIN_TAR" 2>/dev/null; then
    die 'exact-profile vault decryption failed'
fi
unset ARC_VALIDATOR_VAULT_PASSPHRASE
chmod 600 "$PLAIN_TAR"
python3 "$SCRIPT_DIR/validate-validator-vault.py" "$PLAIN_TAR" \
    || die 'decrypted vault failed metadata-only safe-tar validation'

openssl cms -encrypt \
    -binary \
    -outform DER \
    -aes-256-gcm \
    -recip "$RESTORE_CERT" \
    -keyopt rsa_padding_mode:oaep \
    -keyopt rsa_oaep_md:sha256 \
    -in "$PLAIN_TAR" \
    -out "$CANDIDATE_CMS" 2>/dev/null \
    || die 'CMS re-encryption failed'
chmod 600 "$CANDIDATE_CMS"
[ -s "$CANDIDATE_CMS" ] || die 'CMS re-encryption produced an empty file'
openssl cms -cmsout -inform DER -in "$CANDIDATE_CMS" -print -noout \
    > "$CMS_TEXT" 2>/dev/null || die 'CMS output cannot be parsed'
grep -Fq 'algorithm: rsaesOaep' "$CMS_TEXT" \
    || die 'CMS output does not use RSA-OAEP key transport'
grep -Fq 'algorithm: aes-256-gcm' "$CMS_TEXT" \
    || die 'CMS output does not use authenticated AES-256-GCM content encryption'
[ "$(grep -Fc 'OBJECT            :sha256' "$CMS_TEXT")" -ge 2 ] \
    || die 'CMS RSA-OAEP parameters do not use SHA-256'

# Hard-link publication is create-only. The workflow places source, work, and
# output beneath RUNNER_TEMP so crossing a filesystem is an error, not a reason
# to fall back to an overwrite-capable copy.
ln -- "$CANDIDATE_CMS" "$OUTPUT" \
    || die 'could not atomically publish the create-only CMS output'
chmod 600 "$OUTPUT"

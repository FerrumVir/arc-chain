#!/usr/bin/env bash
set -u

# Offline curl stand-in for install-community-node.sh contract tests.
# It intentionally understands only the GitHub and localhost requests the
# installer is allowed to make. An unknown URL fails closed.

original_args="$*"
output_path=""
url=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        -o|--output)
            output_path="$2"
            shift 2
            ;;
        -m|--max-time|--connect-timeout|-r|--range|--retry|--retry-delay|--retry-max-time)
            shift 2
            ;;
        -*)
            shift
            ;;
        http://*|https://*)
            url="$1"
            shift
            ;;
        *)
            shift
            ;;
    esac
done

printf '%s\n' "$original_args" >>"${MOCK_CURL_LOG:?MOCK_CURL_LOG is required}"

emit_file() {
    local source_file="$1"
    if [ -n "$output_path" ]; then
        if [ "$output_path" != /dev/null ]; then
            cp "$source_file" "$output_path"
        fi
    else
        cat "$source_file"
    fi
}

emit_text() {
    local value="$1"
    if [ -n "$output_path" ]; then
        if [ "$output_path" != /dev/null ]; then
            printf '%s\n' "$value" >"$output_path"
        fi
    else
        printf '%s\n' "$value"
    fi
}

render_fake_binary() {
    local version="$1" asset="$2" destination="$3" program descriptor_output
    case "$asset" in
        arc-cli-*) program='arc-cli' ;;
        *)         program='arc-node' ;;
    esac
    {
        printf '#!/usr/bin/env bash\n'
        # shellcheck disable=SC2016 # Generate this literal in the fake executable.
        printf 'if [ "${1:-}" = "--version" ]; then\n'
        printf "    printf '%%s\\n' '%s %s'\n" "$program" "$version"
        printf '    exit 0\n'
        printf 'fi\n'
        if [ "$program" = arc-node ]; then
            descriptor_output="{\"status\":\"VERIFIED_DESCRIPTOR_QUORUM\",\"manifest_hash\":\"$(printf '%064d' 1)\",\"signing_hash\":\"$(printf '%064d' 8)\",\"network_genesis_hash\":\"$(printf '%064d' 3)\",\"recovery_domain\":\"$(printf '%064d' 2)\",\"recovery_epoch\":1,\"validator_set_id\":1,\"source_height\":137145,\"transition_height\":137146,\"validator_count\":6,\"verified_signature_count\":5,\"signed_stake\":33333334,\"total_stake\":40000000}"
            # shellcheck disable=SC2016 # Generate this literal command gate.
            printf 'if [ "${1:-}:${2:-}" = "recovery:verify-descriptor" ]; then\n'
            printf "    printf '%%s\\n' '%s'\n" "$descriptor_output"
            printf '    exit 0\n'
            printf 'fi\n'
            printf 'sha_of() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '\''{print $1}'\''; else shasum -a 256 "$1" | awk '\''{print $1}'\''; fi; }\n'
            # shellcheck disable=SC2016 # Generate the mock retirement CLI literally.
            printf 'if [ "${1:-}:${2:-}" = "legacy-retirement:create-intent" ]; then\n'
            printf '    [ "${MOCK_RETIREMENT_CREATE_FAIL:-0}" != 1 ] || exit 41\n'
            printf '    shift 2; intent=""; pid=""; offline=false\n'
            printf '    while [ "$#" -gt 0 ]; do case "$1" in --intent-output) intent="$2"; shift 2 ;; --legacy-pid) pid="$2"; shift 2 ;; --already-offline) offline=true; shift ;; --forensic-only) shift ;; *) shift 2 ;; esac; done\n'
            printf '    [ -n "$intent" ] || exit 42\n'
            printf '    if [ ! -e "$intent" ]; then (umask 077; printf '\''{"schema":"arc.migration.legacy-v07-community-retirement-intent.v1"}\\n'\'' >"$intent") || exit 43; fi\n'
            printf '    digest="$(sha_of "$intent")" || exit 44\n'
            printf '    if [ -n "$pid" ]; then printf '\''{"legacy_boot_id":"11111111-2222-3333-4444-555555555555","legacy_pid":%%s,"legacy_start_ticks":123,"path":"%%s","retirement_mode":"term_only","schema":"arc.migration.legacy-v07-community-retirement-intent.v1","sha256":"%%s","status":"RETIREMENT_INTENT_CREATED"}\\n'\'' "$pid" "$intent" "$digest"; else printf '\''{"legacy_boot_id":null,"legacy_pid":null,"legacy_start_ticks":null,"path":"%%s","retirement_mode":"preexisting_offline","schema":"arc.migration.legacy-v07-community-retirement-intent.v1","sha256":"%%s","status":"RETIREMENT_INTENT_CREATED"}\\n'\'' "$intent" "$digest"; fi\n'
            printf '    exit 0\n'
            printf 'fi\n'
            # shellcheck disable=SC2016 # Generate the mock retirement finalizer literally.
            printf 'if [ "${1:-}:${2:-}" = "legacy-retirement:finalize" ]; then\n'
            printf '    [ "${MOCK_RETIREMENT_FINALIZE_FAIL:-0}" != 1 ] || exit 51\n'
            printf '    shift 2; receipt=""\n'
            printf '    while [ "$#" -gt 0 ]; do case "$1" in --receipt-output) receipt="$2"; shift 2 ;; *) shift 2 ;; esac; done\n'
            printf '    [ -n "$receipt" ] || exit 52\n'
            printf '    if [ ! -e "$receipt" ]; then (umask 077; printf '\''{"schema":"arc.migration.legacy-v07-community-retirement-receipt.v1"}\\n'\'' >"$receipt") || exit 53; fi\n'
            printf '    digest="$(sha_of "$receipt")" || exit 54\n'
            printf '    printf '\''{"path":"%%s","schema":"arc.migration.legacy-v07-community-retirement-receipt.v1","sha256":"%%s","status":"RETIREMENT_RECEIPT_CREATED"}\\n'\'' "$receipt" "$digest"\n'
            printf '    exit 0\n'
            printf 'fi\n'
        fi
        if [ "$program" = arc-cli ]; then
            # Offline identity behavior used by installer migration tests.
            # The mock never needs the secret; it preserves a deterministic
            # public address for a protected legacy seed and emits the exact
            # JSON shape consumed by the installer contract.
            printf 'if [ "${1:-}" = "keygen" ]; then\n'
            printf '    shift; output=""; legacy=""; verify=""\n'
            printf '    while [ "$#" -gt 0 ]; do case "$1" in --output) output="$2"; shift 2 ;; --legacy-seed-file) legacy="$2"; shift 2 ;; --verify-keyfile) verify="$2"; shift 2 ;; --scheme) shift 2 ;; *) shift ;; esac; done\n'
            printf '    if [ -n "$verify" ]; then address="$(sed -n '\''s/^[[:space:]]*"address": "\\([0-9a-f]*\\)".*/\\1/p'\'' "$verify")"; printf "%%s\\n" "$address"; [ "${#address}" -eq 64 ]; exit; fi\n'
            printf '    [ -n "$output" ] || exit 2\n'
            printf '    if [ -n "$legacy" ]; then if command -v sha256sum >/dev/null 2>&1; then address="$(sed -n '\''1p'\'' "$legacy" | sha256sum | awk '\''{print $1}'\'')"; else address="$(sed -n '\''1p'\'' "$legacy" | shasum -a 256 | awk '\''{print $1}'\'')"; fi; else address=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; fi\n'
            printf '    (umask 077; printf '\''{\\n  "scheme": "ed25519",\\n  "secret_key": "%%064d",\\n  "public_key": "%%064d",\\n  "address": "%%s"\\n}\\n'\'' 0 0 "$address" >"$output") || exit 1\n'
            printf '    exit 0\n'
            printf 'fi\n'
            # shellcheck disable=SC2016 # Expansion belongs to the generated executable.
            printf 'if [ "${3:-}" = "health" ]; then\n'
            # shellcheck disable=SC2016 # Expansion belongs to the generated executable.
            printf '    case "${MOCK_HEALTH_STATUS:-ok}" in ok|degraded) printf "%%s\\n" "$MOCK_HEALTH_STATUS"; exit 0 ;; *) exit 1 ;; esac\n'
            printf 'fi\n'
        fi
        # shellcheck disable=SC2016 # Expansion belongs to the generated executable.
        printf 'printf "%%s\\n" "$*" >>"${ARC_TEST_NODE_ARGS_LOG:?}"\n'
        printf 'exit 0\n'
    } >"$destination"
    chmod +x "$destination"
}

render_asset() {
    local version="$1" asset="$2" destination="$3"
    case "$asset" in
        testnet-seeds.txt)
            printf '# release v%s\n127.0.0.1:19091\n' "$version" >"$destination"
            ;;
        genesis.toml)
            printf '%s\n' \
                '[chain]' \
                "name = \"arc-release-recovered-fixture-v$version\"" \
                'chain_id = "0x415243"' \
                'validator_set_complete = true' \
                'community_rewards_v1_activation_height = 137146' \
                '' \
                '[[accounts]]' \
                'address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' \
                'balance = 0' \
                '' \
                '[[validators]]' \
                'address = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' \
                'stake = 5_000_000' >"$destination"
            ;;
        install.sh)
            printf '#!/usr/bin/env bash\n# release v%s\nexit 0\n' "$version" >"$destination"
            chmod +x "$destination"
            ;;
        arc-cutover-policy.json|arc-legacy-maintenance-boundary.json|arc-recovery-checkpoint-descriptor.json)
            printf '{"asset":"%s","fixture_version":"%s"}\n' "$asset" "$version" >"$destination"
            ;;
        *)
            render_fake_binary "$version" "$asset" "$destination"
            ;;
    esac
}

sha256_of() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    else
        shasum -a 256 "$file" | awk '{print $1}'
    fi
}

asset_is_available() {
    local needle="$1"
    case " ${MOCK_AVAILABLE_ASSETS:-} " in
        *" $needle "*) return 0 ;;
        *)              return 1 ;;
    esac
}

case "$url" in
    https://api.github.com/repos/FerrumVir/arc-chain/releases/latest)
        emit_file "${MOCK_RELEASE_FILE:?MOCK_RELEASE_FILE is required}"
        ;;
    https://api.github.com/repos/FerrumVir/arc-chain/releases/tags/v*)
        requested_tag="${url##*/releases/tags/}"
        exact_fixture="${MOCK_RELEASE_FILE:?MOCK_RELEASE_FILE is required}"
        if [ -n "${MOCK_RELEASE_FIXTURE_DIR:-}" ]; then
            exact_fixture="$MOCK_RELEASE_FIXTURE_DIR/$requested_tag.json"
            [ -f "$exact_fixture" ] || exit 22
        fi
        fixture_tag="$(sed -nE 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$exact_fixture" | head -n1)"
        if [ "$requested_tag" != "$fixture_tag" ]; then
            exit 22
        fi
        emit_file "$exact_fixture"
        ;;
    https://api.github.com/repos/FerrumVir/arc-chain/releases\?per_page=100)
        if [ -n "${MOCK_RELEASE_LIST_FILE:-}" ]; then
            emit_file "$MOCK_RELEASE_LIST_FILE"
        else
            printf '[\n'
            emit_file "${MOCK_RELEASE_FILE:?MOCK_RELEASE_FILE is required}"
            printf ']\n'
        fi
        ;;
    https://api.github.com/repos/FerrumVir/arc-chain/releases\?*)
        exit 22
        ;;
    https://api.github.com/repos/FerrumVir/arc-chain/commits/v*)
        emit_text "{\"sha\":\"${MOCK_RELEASE_COMMIT:?MOCK_RELEASE_COMMIT is required}\"}"
        ;;
    https://raw.githubusercontent.com/FerrumVir/arc-chain/*/testnet-seeds.txt)
        emit_text '127.0.0.1:19091'
        ;;
    https://raw.githubusercontent.com/FerrumVir/arc-chain/*/genesis.toml)
        emit_text $'[chain]\nname = "arc-release-recovered-fixture"\nchain_id = "0x415243"\nvalidator_set_complete = true\ncommunity_rewards_v1_activation_height = 137146\n\n[[accounts]]\naddress = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"\nbalance = 0\n\n[[validators]]\naddress = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"\nstake = 5_000_000'
        ;;
    https://*/network/info)
        retirement_host="${url#https://}"
        retirement_host="${retirement_host%/network/info}"
        case "$retirement_host" in
            149.28.32.76) retirement_validator=adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f ;;
            140.82.16.112) retirement_validator=44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780 ;;
            136.244.109.1) retirement_validator=5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21 ;;
            104.238.171.11) retirement_validator=228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd ;;
            202.182.107.41) retirement_validator=f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f ;;
            149.28.153.31) retirement_validator=f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b ;;
            *) exit 22 ;;
        esac
        retirement_mode="${MOCK_V3_RETIREMENT_MODE:-ok}"
        [ "$retirement_mode" != offline ] || { [ "$retirement_host" != 149.28.153.31 ] || exit 7; }
        retirement_active=true
        retirement_height=137146
        retirement_manifest="0x$(printf '%064d' 1)"
        retirement_network_genesis="0x$(printf '%064d' 3)"
        retirement_node_version=0.8.0
        retirement_protocol_version=0.8.0
        case "$retirement_mode" in
            ok|offline|legacy-listener) ;;
            recovery-inactive) retirement_active=false ;;
            low-height) retirement_height=137145 ;;
            split-manifest)
                [ "$retirement_host" != 149.28.153.31 ] \
                    || retirement_manifest="0x$(printf '%064d' 4)" ;;
            split-network-genesis)
                [ "$retirement_host" != 149.28.153.31 ] \
                    || retirement_network_genesis="0x$(printf '%064d' 5)" ;;
            wrong-validator)
                [ "$retirement_host" != 149.28.153.31 ] \
                    || retirement_validator=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ;;
            old-node) retirement_node_version=0.7.11 ;;
            duplicate-field) ;;
            leading-zero-height) retirement_height=0137146 ;;
            huge-height) retirement_height=9999999999999999999 ;;
            exponent-height) retirement_height=1e9 ;;
            *) exit 22 ;;
        esac
        if [ "$retirement_mode" = duplicate-field ]; then
            emit_text "{\"node_version\":\"$retirement_node_version\",\"protocol_version\":\"$retirement_protocol_version\",\"recovery_active\":$retirement_active,\"recovery_epoch\":1,\"validator_set_id\":1,\"height\":$retirement_height,\"height\":$retirement_height,\"validator_address\":\"0x$retirement_validator\",\"checkpoint_manifest_hash\":\"$retirement_manifest\",\"transaction_domain\":\"0x$(printf '%064d' 2)\",\"genesis_hash\":\"0x$(printf '%064d' 9)\",\"network_genesis_hash\":\"$retirement_network_genesis\"}"
        else
            emit_text "{\"node_version\":\"$retirement_node_version\",\"protocol_version\":\"$retirement_protocol_version\",\"recovery_active\":$retirement_active,\"recovery_epoch\":1,\"validator_set_id\":1,\"height\":$retirement_height,\"validator_address\":\"0x$retirement_validator\",\"checkpoint_manifest_hash\":\"$retirement_manifest\",\"transaction_domain\":\"0x$(printf '%064d' 2)\",\"genesis_hash\":\"0x$(printf '%064d' 9)\",\"network_genesis_hash\":\"$retirement_network_genesis\"}"
        fi
        ;;
    http://*:9090/health|http://*:3001/health)
        retirement_listener="${url#http://}"
        retirement_listener="${retirement_listener%/health}"
        case "$retirement_listener" in
            149.28.32.76:9090|149.28.32.76:3001|140.82.16.112:9090|140.82.16.112:3001|\
            136.244.109.1:9090|136.244.109.1:3001|104.238.171.11:9090|104.238.171.11:3001|\
            202.182.107.41:9090|202.182.107.41:3001|149.28.153.31:9090|149.28.153.31:3001) ;;
            *) exit 22 ;;
        esac
        case "${MOCK_LEGACY_LISTENER_OPEN:-}" in
            all|"$retirement_listener") emit_text '{"status":"legacy-listener"}' ;;
            *) exit 7 ;;
        esac
        ;;
    http://localhost:*/health|http://127.0.0.1:*/health)
        requested_port="$(printf '%s' "$url" | sed -E 's#^http://(localhost|127\.0\.0\.1):([0-9]+)/health$#\2#')"
        if [ "$requested_port" != "${MOCK_HEALTH_PORT:-}" ]; then
            exit 22
        fi
        printf '{"status":"%s","peers":1}\n' "${MOCK_HEALTH_STATUS:-ok}"
        ;;
    https://github.com/FerrumVir/arc-chain/releases/download/v*/*)
        release_tail="${url#*'/releases/download/v'}"
        version="${release_tail%%/*}"
        asset="${release_tail#*/}"
        asset_is_available "$version/$asset" || exit 22

        if [ "$output_path" = /dev/null ]; then
            exit 0
        fi

        temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/arc-mock-curl.XXXXXX")"
        trap 'rm -rf "$temp_dir"' EXIT
        if [ "$asset" = SHA256SUMS ]; then
            [ "${MOCK_MISSING_CHECKSUM:-0}" = 0 ] || exit 22
            manifest="$temp_dir/SHA256SUMS"
            {
                printf '# ARC release manifest v1\n'
                printf '# repository=FerrumVir/arc-chain\n'
                printf '# tag=v%s\n' "$version"
                printf '# commit=%s\n' "${MOCK_RELEASE_COMMIT:?MOCK_RELEASE_COMMIT is required}"
            } >"$manifest"
            for checksum_asset in ${MOCK_CHECKSUM_ASSETS:?MOCK_CHECKSUM_ASSETS is required}; do
                fake="$temp_dir/$checksum_asset"
                render_asset "$version" "$checksum_asset" "$fake"
                printf '%s  %s\n' "$(sha256_of "$fake")" "$checksum_asset" >>"$manifest"
            done
            if [ -n "${MOCK_DUPLICATE_CHECKSUM_ASSET:-}" ]; then
                duplicate_line="$(grep -E \
                    "[[:space:]]${MOCK_DUPLICATE_CHECKSUM_ASSET}$" \
                    "$manifest" | head -n 1)"
                printf '%s\n' "$duplicate_line" >> "$manifest"
            fi
            emit_file "$manifest"
        elif [ "$asset" = SHA256SUMS.sig ]; then
            [ "${MOCK_MISSING_MANIFEST_SIGNATURE:-0}" = 0 ] || exit 22
            if [ "${MOCK_TAMPER_MANIFEST_SIGNATURE:-0}" = 1 ]; then
                emit_text 'ARC TEST TAMPERED SIGNATURE'
            else
                emit_text 'ARC TEST RELEASE SIGNATURE v1'
            fi
        else
            fake="$temp_dir/$asset"
            render_asset "$version" "$asset" "$fake"
            if [ "${MOCK_TAMPER_BINARY:-0}" = 1 ]; then
                printf '# tampered after signing\n' >>"$fake"
            fi
            emit_file "$fake"
        fi
        ;;
    '')
        printf 'mock curl received no URL: %s\n' "$original_args" >&2
        exit 2
        ;;
    *)
        printf 'mock curl refuses unexpected URL: %s\n' "$url" >&2
        exit 22
        ;;
esac

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
    local version="$1" asset="$2" destination="$3" program
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
        if [ "$program" = arc-cli ]; then
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
        fixture_tag="$(sed -nE 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$MOCK_RELEASE_FILE" | head -n1)"
        if [ "$requested_tag" != "$fixture_tag" ]; then
            exit 22
        fi
        emit_file "$MOCK_RELEASE_FILE"
        ;;
    https://api.github.com/repos/FerrumVir/arc-chain/releases\?*)
        # Release walking is intentionally outside the hardened contract.
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

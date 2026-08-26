#!/usr/bin/env bash
set -u

command_name="$(basename "$0")"
case "$command_name" in
    uname)
        case "${1:-}" in
            -s) printf '%s\n' "${TEST_UNAME_S:?TEST_UNAME_S is required}" ;;
            -m) printf '%s\n' "${TEST_UNAME_M:?TEST_UNAME_M is required}" ;;
            *)  printf '%s\n' "${TEST_UNAME_S:?TEST_UNAME_S is required}" ;;
        esac
        ;;
    sleep)
        # Installer startup waits are deliberately elided in offline tests.
        exit 0
        ;;
    free)
        printf '              total        used        free\n'
        printf 'Mem:             16           1          15\n'
        ;;
    sysctl)
        printf '17179869184\n'
        ;;
    openssl)
        if [ "${1:-}" = rand ]; then
            printf '01020304\n'
        else
            exit 2
        fi
        ;;
    hostname)
        printf 'arc-contract-test\n'
        ;;
    id)
        current_uid="${MOCK_CURRENT_UID:-1000}"
        current_user="${MOCK_CURRENT_USER:-arc-community-test}"
        target_user="${MOCK_TARGET_USER:-arc-community-test}"
        target_uid="${MOCK_TARGET_UID:-1000}"
        target_group="${MOCK_TARGET_GROUP:-arc-community-test}"
        case "${1:-}" in
            -u)
                if [ "$#" -gt 1 ]; then printf '%s\n' "$target_uid"; else printf '%s\n' "$current_uid"; fi ;;
            -un) printf '%s\n' "$current_user" ;;
            -gn) printf '%s\n' "$target_group" ;;
            '')  printf 'uid=%s(%s) gid=%s(%s)\n' "$current_uid" "$current_user" "$target_uid" "$target_group" ;;
            "$current_user"|"$target_user") exit 0 ;;
            *) exit 1 ;;
        esac
        ;;
    getent)
        if [ "${1:-}" = passwd ] && [ "${2:-}" = "${MOCK_TARGET_USER:-arc-community-test}" ]; then
            printf '%s:x:%s:%s:ARC test user:%s:/bin/bash\n' \
                "${MOCK_TARGET_USER:-arc-community-test}" \
                "${MOCK_TARGET_UID:-1000}" \
                "${MOCK_TARGET_UID:-1000}" \
                "${MOCK_TARGET_HOME:?MOCK_TARGET_HOME is required}"
        else
            exit 2
        fi
        ;;
    chown)
        printf 'chown %s\n' "$*" >>"${MOCK_OWNER_LOG:?MOCK_OWNER_LOG is required}"
        exit 0
        ;;
    sudo|systemctl|launchctl)
        printf '%s %s\n' "$command_name" "$*" >>"${MOCK_SERVICE_LOG:?MOCK_SERVICE_LOG is required}"
        exit 0
        ;;
    *)
        printf 'unsupported mock command name: %s\n' "$command_name" >&2
        exit 2
        ;;
esac

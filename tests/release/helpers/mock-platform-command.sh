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
        if [ "${MOCK_REAL_SLEEP:-false}" = true ]; then
            /bin/sleep 0.05
        fi
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
    ssh-keygen)
        case " $* " in
            *' -Y verify '*)
                signature_file=''
                while [ "$#" -gt 0 ]; do
                    if [ "$1" = -s ] && [ "$#" -gt 1 ]; then
                        signature_file="$2"
                        break
                    fi
                    shift
                done
                [ -n "$signature_file" ] \
                    && grep -Fxq 'ARC TEST RELEASE SIGNATURE v1' "$signature_file"
                ;;
            *) exit 2 ;;
        esac
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
    stat)
        # Contract tests simulate uid 1000 even when the host test account has
        # another uid. Preserve real stat behavior except for the managed
        # install-root marker ownership probe.
        last_argument="${!#}"
        if [ -n "${MOCK_ROOT_OWNED_PREFIX:-}" ]; then
            case " $* :$last_argument" in
                *' %u '*":${MOCK_ROOT_OWNED_PREFIX}"|*' %u '*":${MOCK_ROOT_OWNED_PREFIX}/"*)
                    printf '0\n'
                    exit 0
                    ;;
            esac
        fi
        case " $* " in
            *' -c %u -- '*'.arc-chain-install-root '*|*' -f %u '*'.arc-chain-install-root '*)
                printf '%s\n' "${MOCK_INSTALL_MARKER_UID:-${MOCK_TARGET_UID:-1000}}"
                ;;
            *) /usr/bin/stat "$@" ;;
        esac
        ;;
    runuser)
        # The installer uses runuser only to prove the selected community user
        # can access its directories. The contract test cannot create an OS
        # account, so consume `-u USER --` and execute the probe unchanged.
        [ "${1:-}" = -u ] && [ "$#" -ge 4 ] || exit 2
        shift 2
        [ "${1:-}" = -- ] || exit 2
        shift
        "$@"
        ;;
    sudo)
        printf '%s %s\n' "$command_name" "$*" >>"${MOCK_SERVICE_LOG:?MOCK_SERVICE_LOG is required}"
        if [ "${1:-}" = -v ]; then exit 0; fi
        if [ "${MOCK_SUDO_EXECUTE:-false}" = true ]; then
            "$@"
        else
            exit 0
        fi
        ;;
    systemctl)
        printf '%s %s\n' "$command_name" "$*" >>"${MOCK_SERVICE_LOG:?MOCK_SERVICE_LOG is required}"
        arguments=" $* "
        if [ -n "${MOCK_SERVICE_FAIL_MATCH:-}" ]; then
            case "$arguments" in
                *"${MOCK_SERVICE_FAIL_MATCH}"*)
                    marker="${MOCK_SERVICE_FAIL_ONCE_FILE:?MOCK_SERVICE_FAIL_ONCE_FILE is required when failure injection is active}"
                    if [ ! -e "$marker" ]; then
                        : >"$marker"
                        exit 1
                    fi
                    ;;
                esac
        fi
        state_dir="${MOCK_SYSTEMD_STATE_DIR:-}"
        [ -z "$state_dir" ] || mkdir -p "$state_dir"
        systemd_default_state() {
            state_kind="$1" state_unit="$2"
            case "$state_unit:$state_kind" in
                arc-node.service:active) printf '%s\n' "${MOCK_SYSTEMD_NODE_ACTIVE:-false}" ;;
                arc-node.service:enabled) printf '%s\n' "${MOCK_SYSTEMD_NODE_ENABLED:-false}" ;;
                arc-node-update.timer:active) printf '%s\n' "${MOCK_SYSTEMD_UPDATER_ACTIVE:-false}" ;;
                arc-node-update.timer:enabled) printf '%s\n' "${MOCK_SYSTEMD_UPDATER_ENABLED:-false}" ;;
                arc-updater.timer:active) printf '%s\n' "${MOCK_LEGACY_UPDATER_ACTIVE:-false}" ;;
                arc-updater.timer:enabled) printf '%s\n' "${MOCK_LEGACY_UPDATER_ENABLED:-false}" ;;
                arc-updater.service:active) printf '%s\n' "${MOCK_LEGACY_UPDATER_SERVICE_ACTIVE:-false}" ;;
                arc-updater.service:enabled) printf '%s\n' "${MOCK_LEGACY_UPDATER_SERVICE_ENABLED:-false}" ;;
                *) printf 'false\n' ;;
            esac
        }
        systemd_read_state() {
            state_kind="$1" state_unit="$2"
            state_file="$state_dir/${state_unit}.${state_kind}"
            if [ -n "$state_dir" ] && [ -f "$state_file" ]; then
                sed -n '1p' "$state_file"
            else
                systemd_default_state "$state_kind" "$state_unit"
            fi
        }
        systemd_write_state() {
            state_kind="$1" state_unit="$2" state_value="$3"
            [ -z "$state_dir" ] || printf '%s\n' "$state_value" >"$state_dir/${state_unit}.${state_kind}"
        }
        set -- "$@"
        [ "${1:-}" != --user ] || shift
        action="${1:-}"
        shift || true
        case "$action" in
            show-environment) exit 0 ;;
            show)
                property=''
                for candidate in "$@"; do
                    case "$candidate" in
                        --property=*) property="${candidate#--property=}" ;;
                    esac
                done
                restart_delay="${MOCK_SYSTEMD_RESTART_DELAY_POLLS:-}"
                restart_counter_file="$state_dir/arc-node.service.restart-polls"
                restart_seen_file="$state_dir/arc-node.service.restart-mainpid-seen"
                restart_polls=0
                if [ -n "$state_dir" ] && [ -f "$restart_counter_file" ]; then
                    restart_polls="$(sed -n '1p' "$restart_counter_file")"
                fi
                case "$property" in
                    MainPID)
                        [ -n "${MOCK_SYSTEMD_MAIN_PID:-}" ] || exit 1
                        if [ -n "$restart_delay" ] && [ -n "$state_dir" ]; then
                            if [ -f "$restart_seen_file" ]; then
                                restart_polls=$((restart_polls + 1))
                                printf '%s\n' "$restart_polls" >"$restart_counter_file"
                            else
                                : >"$restart_seen_file"
                            fi
                        fi
                        if [ -n "$restart_delay" ] && [ "$restart_polls" -gt "$restart_delay" ]; then
                            printf '%s\n' "${MOCK_SYSTEMD_MAIN_PID_AFTER:?delayed restart requires MOCK_SYSTEMD_MAIN_PID_AFTER}"
                        else
                            printf '%s\n' "$MOCK_SYSTEMD_MAIN_PID"
                        fi
                        ;;
                    ActiveState)
                        if [ -n "$restart_delay" ] && [ "$restart_polls" -gt 0 ] \
                            && [ "$restart_polls" -le "$restart_delay" ]; then
                            printf 'deactivating\n'
                        elif [ -n "$restart_delay" ] && [ "$restart_polls" -gt "$restart_delay" ]; then
                            printf 'active\n'
                        elif [ "${MOCK_SYSTEMD_NODE_ACTIVE:-false}" = true ]; then
                            printf 'active\n'
                        else
                            printf 'inactive\n'
                        fi
                        ;;
                    SubState)
                        if [ -n "$restart_delay" ] && [ "$restart_polls" -gt 0 ] \
                            && [ "$restart_polls" -le "$restart_delay" ]; then
                            printf 'stop-sigterm\n'
                        elif [ -n "$restart_delay" ] && [ "$restart_polls" -gt "$restart_delay" ]; then
                            printf 'running\n'
                        elif [ "${MOCK_SYSTEMD_NODE_ACTIVE:-false}" = true ]; then
                            printf 'running\n'
                        else
                            printf 'dead\n'
                        fi
                        ;;
                    *) exit 1 ;;
                esac
                exit 0
                ;;
            is-active|is-enabled)
                state_kind=active
                [ "$action" != is-enabled ] || state_kind=enabled
                for candidate in "$@"; do
                    case "$candidate" in --quiet|--*) continue ;; esac
                    state_unit="$candidate"
                done
                if [ "$(systemd_read_state "$state_kind" "$state_unit")" = true ]; then
                    exit 0
                fi
                exit 1
                ;;
            start|restart|stop|enable|disable)
                now=false
                for candidate in "$@"; do
                    case "$candidate" in
                        --now) now=true ;;
                        --*) ;;
                        *)
                            case "$action" in
                                start|restart) systemd_write_state active "$candidate" true ;;
                                stop) systemd_write_state active "$candidate" false ;;
                                enable)
                                    systemd_write_state enabled "$candidate" true
                                    [ "$now" = false ] || systemd_write_state active "$candidate" true ;;
                                disable)
                                    systemd_write_state enabled "$candidate" false
                                    [ "$now" = false ] || systemd_write_state active "$candidate" false ;;
                            esac
                            ;;
                    esac
                done
                exit 0
                ;;
        esac
        exit 0
        ;;
    launchctl)
        printf '%s %s\n' "$command_name" "$*" >>"${MOCK_SERVICE_LOG:?MOCK_SERVICE_LOG is required}"
        launchd_state_dir="${MOCK_LAUNCHD_STATE_DIR:-}"
        [ -z "$launchd_state_dir" ] || mkdir -p "$launchd_state_dir"
        launchd_label_state() {
            launchd_label="$1"
            launchd_file="$launchd_state_dir/${launchd_label}.loaded"
            if [ -n "$launchd_state_dir" ] && [ -f "$launchd_file" ]; then
                sed -n '1p' "$launchd_file"
                return
            fi
            case "$launchd_label" in
                network.arc.node) printf '%s\n' "${MOCK_LAUNCHD_NODE_LOADED:-false}" ;;
                network.arc.update) printf '%s\n' "${MOCK_LAUNCHD_UPDATER_LOADED:-false}" ;;
                com.arc.inference) printf '%s\n' "${MOCK_LEGACY_LAUNCHD_NODE_LOADED:-false}" ;;
                com.arc.updater) printf '%s\n' "${MOCK_LEGACY_LAUNCHD_UPDATER_LOADED:-false}" ;;
                *) printf 'false\n' ;;
            esac
        }
        launchd_set_state() {
            launchd_label="$1" launchd_value="$2"
            [ -z "$launchd_state_dir" ] \
                || printf '%s\n' "$launchd_value" >"$launchd_state_dir/${launchd_label}.loaded"
        }
        launchd_label_pid() {
            case "$1" in
                network.arc.node) printf '%s\n' "${MOCK_LAUNCHD_NODE_PID:-}" ;;
                com.arc.inference) printf '%s\n' "${MOCK_LEGACY_LAUNCHD_NODE_PID:-}" ;;
                *) printf '\n' ;;
            esac
        }
        case " $* " in
            *' print-disabled '*)
                printf 'disabled services = {\n'
                [ "${MOCK_LAUNCHD_NODE_DISABLED:-false}" = true ] \
                    && printf '    "network.arc.node" => true\n'
                [ "${MOCK_LAUNCHD_UPDATER_DISABLED:-false}" = true ] \
                    && printf '    "network.arc.update" => true\n'
                printf '}\n'
                exit 0
                ;;
            *' print '*)
                launchd_target="${2:-}"
                launchd_label="${launchd_target##*/}"
                case "$launchd_target" in
                    gui/[0-9]*|user/[0-9]*)
                        case "$launchd_target" in
                            */network.arc.node|*/network.arc.update|*/com.arc.inference|*/com.arc.updater)
                                if [ "$(launchd_label_state "$launchd_label")" = true ]; then
                                    launchd_pid="$(launchd_label_pid "$launchd_label")"
                                    if [ -n "$launchd_pid" ] \
                                        && kill -0 "$launchd_pid" 2>/dev/null; then
                                        printf '    pid = %s\n' "$launchd_pid"
                                    fi
                                    exit 0
                                fi
                                exit 1 ;;
                            *)
                                if [ "${MOCK_LAUNCHD_GUI_AVAILABLE:-true}" = true ]; then
                                    exit 0
                                fi
                                exit 1 ;;
                        esac
                        ;;
                esac
                ;;
            *' bootout '*)
                launchd_target="${2:-}"
                launchd_set_state "${launchd_target##*/}" false
                exit 0
                ;;
            *' bootstrap '*)
                launchd_plist="${3:-}"
                case "$launchd_plist" in
                    *com.arc.inference.plist) launchd_set_state com.arc.inference true ;;
                    *com.arc.updater.plist) launchd_set_state com.arc.updater true ;;
                    *network.arc.node.plist) launchd_set_state network.arc.node true ;;
                    *network.arc.update.plist) launchd_set_state network.arc.update true ;;
                esac
                exit 0
                ;;
        esac
        exit 0
        ;;
    ps)
        case " $* " in
            *' -o command='*)
                if [ -n "${MOCK_PS_COMMAND:-}" ]; then
                    printf '%s\n' "$MOCK_PS_COMMAND"
                else
                    /bin/ps "$@"
                fi
                ;;
            *) /bin/ps "$@" ;;
        esac
        ;;
    *)
        printf 'unsupported mock command name: %s\n' "$command_name" >&2
        exit 2
        ;;
esac

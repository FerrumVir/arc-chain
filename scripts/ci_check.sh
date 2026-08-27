#!/usr/bin/env bash
# ARC Chain local quality gate.
#
# Default (`--full`) mirrors every blocking Linux CI check. `--quick` keeps the
# cheap contracts, linters, compile check, and unit tests for a shorter edit
# loop. Every check is isolated and summarized so one failure does not hide the
# rest of the evidence.
set -uo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT" || exit 1

PROFILE=full
case "${1:-}" in
    ""|--full) PROFILE=full ;;
    --quick) PROFILE=quick ;;
    -h|--help)
        printf 'usage: %s [--full|--quick]\n' "$0"
        printf '  --full   all blocking Linux CI gates (default)\n'
        printf '  --quick  contracts, linters, compile check, and unit tests\n'
        exit 0
        ;;
    *)
        printf 'error: unknown argument: %s\n' "$1" >&2
        printf 'usage: %s [--full|--quick]\n' "$0" >&2
        exit 2
        ;;
esac

PASS=0
FAIL=0
CHECK_INDEX=0
RESULTS=()
LOG_DIR="${ARC_CI_LOG_DIR:-$REPO_ROOT/target/ci-check}"
mkdir -p "$LOG_DIR"

run_check() {
    local name="$1"
    shift
    CHECK_INDEX=$((CHECK_INDEX + 1))
    local log_file
    log_file="$LOG_DIR/$(printf '%02d' "$CHECK_INDEX").log"

    printf '\n━━━ %s ━━━\n' "$name"
    if "$@" >"$log_file" 2>&1; then
        tail -n 20 "$log_file"
        PASS=$((PASS + 1))
        RESULTS+=("[PASS] $name")
    else
        tail -n 50 "$log_file"
        FAIL=$((FAIL + 1))
        RESULTS+=("[FAIL] $name ($log_file)")
    fi
}

require_commands() {
    local missing=0
    local command_name
    for command_name in actionlint cargo git node npm python3 shellcheck; do
        if ! command -v "$command_name" >/dev/null 2>&1; then
            printf 'missing required command: %s\n' "$command_name" >&2
            missing=1
        fi
    done
    if command -v node >/dev/null 2>&1; then
        local node_major
        node_major="$(node -p 'process.versions.node.split(".")[0]')"
        if [ "$node_major" != 24 ]; then
            printf 'Node.js 24 LTS is required for local/CI parity (found %s). Use .node-version.\n' \
                "$(node --version)" >&2
            missing=1
        fi
    fi
    if command -v actionlint >/dev/null 2>&1; then
        local actionlint_version
        actionlint_version="$(actionlint -version 2>/dev/null | head -n 1)"
        if [ "$actionlint_version" != 1.7.12 ]; then
            printf 'Actionlint 1.7.12 is required for local/CI parity (found %s).\n' \
                "${actionlint_version:-unknown}" >&2
            missing=1
        fi
    fi
    return "$missing"
}

check_shell_syntax() {
    local status=0
    local file
    bash -n install.sh || status=1
    while IFS= read -r -d '' file; do
        bash -n "$file" || status=1
    done < <(find scripts deploy tests/release -type f -name '*.sh' -print0)
    return "$status"
}

check_shell_lint() {
    local files=(install.sh)
    local file
    while IFS= read -r -d '' file; do
        files+=("$file")
    done < <(find scripts deploy tests/release -type f -name '*.sh' -print0)
    shellcheck -S warning "${files[@]}"
}

check_workflows() {
    actionlint
}

dashboard_contract() {
    (cd dashboard && npm ci && npm test)
}

desktop_install() {
    (cd desktop && npm ci)
}

desktop_typecheck() {
    (cd desktop && npx tsc --noEmit)
}

desktop_e2e() {
    (
        cd desktop &&
            npx playwright install chromium &&
            CI=true npx playwright test --config playwright.gate.config.ts
    )
}

desktop_tauri_tests() {
    # `tauri::generate_context!()` validates frontendDist at compile time.
    # Build it here just as CI does so this gate also works from a pristine
    # checkout instead of succeeding only when a developer has stale dist/.
    (
        cd desktop &&
            npm run build
    )
    cargo +stable test --manifest-path desktop/src-tauri/Cargo.toml --all-targets --locked
}

sdk_package() {
    (
        cd sdk/typescript &&
            npm ci &&
            npm test
    )
}

compatibility_typescript_sdk() {
    (
        cd sdks/typescript &&
            npm ci &&
            npm audit --audit-level=low &&
            npm test -- --runInBand &&
            npm run build &&
            node -e "const sdk=require('./dist'); if (!sdk || typeof sdk !== 'object') throw new Error('CommonJS SDK failed to load')"
    )
}

python_sdk() {
    (
        local arc_python_venv
        arc_python_venv="$(mktemp -d "${TMPDIR:-/tmp}/arc-python-sdk.XXXXXX")" || exit 1
        trap 'rm -rf -- "$arc_python_venv"' EXIT
        python3 -m venv "$arc_python_venv" &&
            cd sdks/python &&
            "$arc_python_venv/bin/python" -m pip install -e '.[dev]' &&
            "$arc_python_venv/bin/python" -m pip freeze --exclude-editable > "$arc_python_venv/requirements.txt" &&
            "$arc_python_venv/bin/python" -m pip_audit -r "$arc_python_venv/requirements.txt" &&
            "$arc_python_venv/bin/python" -m ruff check arc_sdk tests &&
            "$arc_python_venv/bin/python" -m ruff format --check arc_sdk tests &&
            "$arc_python_venv/bin/python" -m unittest discover -s tests -v &&
            "$arc_python_venv/bin/python" -m compileall -q arc_sdk tests
    )
}

printf '================================================================\n'
printf ' ARC Chain quality gate (%s)\n' "$PROFILE"
printf ' Logs: %s\n' "$LOG_DIR"
printf '================================================================\n'

run_check "Toolchain preflight" require_commands
run_check "Release + installer contracts" bash tests/release/run.sh
run_check "Shell syntax" check_shell_syntax
run_check "ShellCheck" check_shell_lint
run_check "GitHub Actions syntax" check_workflows
run_check "Dashboard build + contract" dashboard_contract
run_check "Explorer contract" node explorer/test-contract.mjs
run_check "Rust formatting" ./scripts/rustfmt-workspace.sh --check
run_check "Workspace compilation" cargo check --workspace --all-targets --locked
run_check "Clippy" cargo clippy --workspace --all-targets --locked -- -D warnings
run_check "Workspace library tests" cargo test --workspace --lib --locked

if [ "$PROFILE" = full ]; then
    run_check "Releasable-worktree secret scan" bash tests/release/current_tree_secret_scan.sh --worktree
    run_check "Workspace integration tests" cargo test --workspace --test '*' --locked -- --test-threads=1
    run_check "Workspace documentation tests" cargo test --workspace --doc --locked
    run_check "Desktop dependency install" desktop_install
    run_check "Desktop typecheck" desktop_typecheck
    run_check "Desktop Playwright E2E" desktop_e2e
    run_check "Desktop Tauri Rust tests" desktop_tauri_tests
    run_check "TypeScript SDK package" sdk_package
    run_check "Compatibility TypeScript SDK" compatibility_typescript_sdk
    run_check "Python SDK" python_sdk
fi

printf '\n================================================================\n'
printf ' RESULTS\n'
printf '================================================================\n'
printf ' %s\n' "${RESULTS[@]}"
printf '\n Passed: %d  Failed: %d\n' "$PASS" "$FAIL"
if [ "$FAIL" -eq 0 ]; then
    printf ' STATUS: ALL REQUIRED CHECKS PASSED\n'
else
    printf ' STATUS: %d CHECK(S) FAILED\n' "$FAIL"
fi
printf '================================================================\n'

[ "$FAIL" -eq 0 ]

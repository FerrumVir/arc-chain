#!/usr/bin/env bash

# Tiny dependency-free test helpers for the release contract suite.
# The caller intentionally controls `set` flags so an assertion failure can be
# reported without aborting the rest of the suite.

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

test_pass() {
    TESTS_PASSED=$((TESTS_PASSED + 1))
    printf 'ok %d - %s\n' "$TESTS_RUN" "$1"
}

test_fail() {
    TESTS_FAILED=$((TESTS_FAILED + 1))
    printf 'not ok %d - %s\n' "$TESTS_RUN" "$1"
    if [ $# -gt 1 ] && [ -n "$2" ]; then
        printf '  ---\n'
        printf '  message: %s\n' "$2"
        printf '  ...\n'
    fi
}

run_test() {
    local name="$1"
    shift
    local detail_file status detail
    if [ -n "${ARC_TEST_FILTER:-}" ] \
        && ! printf '%s\n' "$name" | grep -Eq -- "$ARC_TEST_FILTER"; then
        return 0
    fi
    detail_file="$(mktemp "${TMPDIR:-/tmp}/arc-release-test.XXXXXX")"
    TESTS_RUN=$((TESTS_RUN + 1))

    "$@" >"$detail_file" 2>&1
    status=$?
    detail="$(cat "$detail_file")"
    rm -f "$detail_file"

    if [ "$status" -eq 0 ]; then
        test_pass "$name"
    else
        test_fail "$name" "$detail"
    fi
}

assert_file_contains() {
    local file="$1" pattern="$2" message="$3"
    if ! grep -Eq -- "$pattern" "$file"; then
        printf '%s\n' "$message"
        return 1
    fi
}

assert_file_not_contains() {
    local file="$1" pattern="$2" message="$3"
    if grep -Eq -- "$pattern" "$file"; then
        printf '%s\n' "$message"
        grep -En -- "$pattern" "$file" | sed -n '1,8p'
        return 1
    fi
}

assert_equals() {
    local expected="$1" actual="$2" message="$3"
    if [ "$expected" != "$actual" ]; then
        printf '%s (expected=%q actual=%q)\n' "$message" "$expected" "$actual"
        return 1
    fi
}

assert_nonempty() {
    local actual="$1" message="$2"
    if [ -z "$actual" ]; then
        printf '%s\n' "$message"
        return 1
    fi
}

finish_tests() {
    printf '1..%d\n' "$TESTS_RUN"
    printf '# pass=%d fail=%d\n' "$TESTS_PASSED" "$TESTS_FAILED"
    [ "$TESTS_FAILED" -eq 0 ]
}

#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

BENCH_NODE="$REPO_ROOT/crates/arc-bench/src/node_bench.rs"
BENCH_DASHBOARD="$REPO_ROOT/crates/arc-bench/src/dashboard.html"
BENCH_MANIFEST="$REPO_ROOT/crates/arc-bench/Cargo.toml"
BENCH_EPHEMERAL_KEYS="$REPO_ROOT/crates/arc-bench/src/ephemeral_keys.rs"
BENCH_README="$REPO_ROOT/crates/arc-bench/README.md"
PAPER="$REPO_ROOT/papers/foundations-trustworthy-ai.typ"
NODE_MANIFEST="$REPO_ROOT/crates/arc-node/Cargo.toml"
NODE_MAIN="$REPO_ROOT/crates/arc-node/src/main.rs"
NODE_BENCHMARK="$REPO_ROOT/crates/arc-node/src/benchmark.rs"
CRYPTO_MANIFEST="$REPO_ROOT/crates/arc-crypto/Cargo.toml"
CRYPTO_LIB="$REPO_ROOT/crates/arc-crypto/src/lib.rs"
CRYPTO_SIGNATURE="$REPO_ROOT/crates/arc-crypto/src/signature.rs"
STATE_MANIFEST="$REPO_ROOT/crates/arc-state/Cargo.toml"
STATE_LIB="$REPO_ROOT/crates/arc-state/src/lib.rs"
VALIDATOR_IDENTITY="$REPO_ROOT/crates/arc-node/src/validator_identity.rs"
DESKTOP_IDENTITY="$REPO_ROOT/desktop/src-tauri/src/identity.rs"
DESKTOP_NODE_MANAGER="$REPO_ROOT/desktop/src-tauri/src/node_manager.rs"
DESKTOP_STORE="$REPO_ROOT/desktop/src-tauri/src/store.rs"
CLI_KEYGEN="$REPO_ROOT/crates/arc-cli/src/keygen.rs"
CRYPTO_SECRET_FILE="$REPO_ROOT/crates/arc-crypto/src/secret_file.rs"
INSTALLER="$REPO_ROOT/install.sh"
DESKTOP_FIRST_RUN="$REPO_ROOT/desktop/FIRST-RUN.md"
DESKTOP_ARCHIVED_GAPS="$REPO_ROOT/desktop/PRODUCTION_GAPS.md"
LOCAL_MUTATOR="$REPO_ROOT/crates/arc-node/examples/v070_e2e_attestation.rs"
LOCAL_GUARD="$REPO_ROOT/crates/arc-node/examples/support/local_rpc.rs"
RETIRED_EXAMPLES=(
    diag_model_reg
    diag_open
    keepalive
    live_milestones_cde
    live_paid_inference
    manual_release
    quick_transfer_test
    v079_signed_inference
)

benchmark_never_serves_or_advertises_remote_shell_bootstrap() {
    assert_file_not_contains "$BENCH_NODE" \
        'join[.]sh|sh[.]rustup[.]rs|git[[:space:]]+clone|curl[^|]*[|][[:space:]]*(ba)?sh' \
        'active benchmark node still contains a network-to-shell bootstrap' || return 1
    assert_file_not_contains "$BENCH_DASHBOARD" \
        'join[.]sh|sh[.]rustup[.]rs|git[[:space:]]+clone|curl[^|]*[|][[:space:]]*(ba)?sh' \
        'active benchmark dashboard still advertises a network-to-shell bootstrap' || return 1
    assert_file_contains "$BENCH_DASHBOARD" \
        'arc-bench-node worker --coord' \
        'benchmark dashboard does not require a prebuilt local worker' || return 1
    for literal in \
        'const PUBLIC_BIND_OPT_IN: &str = "--allow-public-benchmark-bind"' \
        '.unwrap_or("127.0.0.1")' \
        '.parse::<IpAddr>()' \
        'if !ip.is_loopback() && opt_in_count == 0' \
        'SocketAddr::new(ip, port)'
    do
        grep -Fq -- "$literal" "$BENCH_NODE" || {
            printf 'benchmark bind path omits a fail-closed invariant: %s\n' "$literal"
            return 1
        }
    done
}

paper_reproduction_is_source_and_toolchain_pinned() {
    assert_file_not_contains "$PAPER" \
        'git[[:space:]]+clone|sh[.]rustup[.]rs|curl[^|]*[|][[:space:]]*(ba)?sh' \
        'paper reproduction path still executes mutable source or bootstrap code' || return 1
    for literal in \
        'ARC_SOURCE_REV=cfb4780030c76b79b4e16ebf912882102cf30192' \
        'ARC_RUST_TOOLCHAIN=nightly-2025-05-31' \
        "git -C arc-chain fetch --depth 1 origin \"\$ARC_SOURCE_REV\"" \
        "git -C arc-chain checkout --detach \"\$ARC_SOURCE_REV\"" \
        "test \"\$(git -C arc-chain rev-parse HEAD)\" = \"\$ARC_SOURCE_REV\"" \
        "rustup run \"\$ARC_RUST_TOOLCHAIN\" rustc --version"
    do
        grep -Fq -- "$literal" "$PAPER" || {
            printf 'paper reproduction path omits immutable prerequisite: %s\n' "$literal"
            return 1
        }
    done
}

documentation_has_no_mutable_executable_bootstrap() {
    local pattern
    pattern='curl[^|]*[|][[:space:]]*(ba)?sh|wget[^|]*[|][[:space:]]*(ba)?sh|sh[.]rustup[.]rs|git[[:space:]]+clone|raw[.]githubusercontent[.]com/[^/]+/[^/]+/(main|master)/|github[.]com/[^/]+/[^/]+/releases/latest/download|/resolve/(main|master)/'
    if grep -REn -- "$pattern" "$REPO_ROOT/docs" "$PAPER"; then
        printf '%s\n' 'documentation contains a mutable executable/model bootstrap'
        return 1
    fi
    if grep -REn -- '--validator-seed|ARC_VALIDATOR_SEED|identity/validator-seed|node[.]env' \
        "$REPO_ROOT/docs" "$PAPER"; then
        printf '%s\n' 'active documentation contains a seed/env identity launch path'
        return 1
    fi
    if grep -Ein -- \
        'using your recovery phrase as the validator seed|generates? a unique validator seed|pulls? the newest pre-built binary|ARM Linux is not covered|latest pre-built binary|validator seed for your machine|recovery phrase is present because the node must sign after a restart|private local store so the node can restart' \
        "$REPO_ROOT/docs" "$DESKTOP_FIRST_RUN" "$REPO_ROOT/README.md" "$PAPER"; then
        printf '%s\n' 'active documentation contains stale mutable-release or seed-identity guidance'
        return 1
    fi
    grep -Fq -- '> **Historical gap list, not current release status.**' \
        "$DESKTOP_ARCHIVED_GAPS" || {
        printf '%s\n' 'archived desktop production gaps lost their stale-content warning'
        return 1
    }
}

cargo_git_dependencies_use_exact_revisions() {
    python3 - "$REPO_ROOT" <<'PY' || return 1
import os
import pathlib
import re
import subprocess
import sys
import tomllib

root = pathlib.Path(sys.argv[1])
full_revision = re.compile(r"[0-9a-f]{40}").fullmatch

def walk(value, location):
    if isinstance(value, dict):
        if "git" in value:
            revision = value.get("rev")
            if not isinstance(revision, str) or not full_revision(revision):
                raise SystemExit(f"git dependency lacks a lowercase full 40-hex rev at {location}")
            for mutable in ("tag", "branch"):
                if mutable in value:
                    raise SystemExit(f"git dependency retains mutable {mutable} selector at {location}")
        for key, nested in value.items():
            walk(nested, f"{location}.{key}")
    elif isinstance(value, list):
        for index, nested in enumerate(value):
            walk(nested, f"{location}[{index}]")

inventory = subprocess.run(
    [
        "git", "-C", os.fspath(root), "ls-files", "-z", "--cached", "--others",
        "--exclude-standard", "--", "Cargo.toml", "**/Cargo.toml",
    ],
    check=True,
    stdout=subprocess.PIPE,
).stdout
for relative in inventory.split(b"\0"):
    if not relative:
        continue
    manifest_path = root / os.fsdecode(relative)
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise SystemExit(f"Cargo manifest is not a regular file: {relative!r}")
    walk(tomllib.loads(manifest_path.read_text()), str(manifest_path.relative_to(root)))
PY
}

python_build_backend_is_exactly_pinned() {
    python3 - "$REPO_ROOT/sdks/python/pyproject.toml" <<'PY' || return 1
import pathlib
import re
import sys
import tomllib

manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
build = manifest.get("build-system", {})
requirements = build.get("requires", [])
if build.get("build-backend") != "hatchling.build":
    raise SystemExit("Python SDK uses an unreviewed build backend")
if not requirements or not any(item == "hatchling==1.27.0" for item in requirements):
    raise SystemExit("Python SDK hatchling backend is not pinned to the reviewed version")
exact = re.compile(
    r"[A-Za-z0-9_.-]+==[A-Za-z0-9_.+!-]+"
    r"(?:; python_version (?:<|<=|==|!=|>=|>) '[0-9.]+')?$"
)
for requirement in requirements:
    if not isinstance(requirement, str) or not exact.fullmatch(requirement):
        raise SystemExit(
            f"Python executable build dependency is not an exact version pin: {requirement!r}"
        )
PY
}

persistent_identity_is_file_only_and_no_replace() {
    for literal in \
        'metadata.uid()' \
        'libc::geteuid()' \
        'create_new_private_directory' \
        'validate_private_directory' \
        'SE_DACL_PROTECTED'
    do
        grep -Fq -- "$literal" "$CRYPTO_SECRET_FILE" || {
            printf 'private file/directory boundary omits ownership invariant: %s\n' "$literal"
            return 1
        }
    done
    for literal in \
        'KeyPair::generate_ed25519()' \
        'IdentitySource::EphemeralLoopbackObserver' \
        'changes on every restart'
    do
        grep -Fq -- "$literal" "$VALIDATOR_IDENTITY" || {
            printf 'node identity policy omits invariant: %s\n' "$literal"
            return 1
        }
    done
    for literal in \
        'rpc_socket.ip().is_loopback()' \
        'peer.ip().is_loopback()' \
        'persistent node identity is required' \
        'fn validate_identity_runtime(' \
        'if seed_configured {' \
        'strictly_loopback && !persistent_role'
    do
        grep -Fq -- "$literal" "$NODE_MAIN" || {
            printf 'node identity runtime omits invariant: %s\n' "$literal"
            return 1
        }
    done

    assert_file_contains "$DESKTOP_IDENTITY" 'ensure_validator_keyfile' \
        'desktop does not materialize a persistent validator keyfile' || return 1
    assert_file_contains "$DESKTOP_IDENTITY" 'fs::hard_link\(&sidecar, &target\)' \
        'desktop keyfile publication is not atomic no-replace' || return 1
    assert_file_contains "$DESKTOP_NODE_MANAGER" '"--validator-key-file"' \
        'desktop node launcher does not pass the persistent keyfile' || return 1
    assert_file_not_contains "$DESKTOP_NODE_MANAGER" \
        'cmd[.]arg[(]"--validator-seed"|cmd[.]env[(]"ARC_VALIDATOR_SEED"' \
        'desktop node launcher still passes phrase material to a new node' || return 1
    for literal in \
        'struct LegacyWindowsStopContext' \
        'validator_seed: Zeroizing<String>' \
        'constant_time_legacy_seed_eq' \
        'configure_legacy_windows_stop_context'
    do
        grep -Fq -- "$literal" "$DESKTOP_NODE_MANAGER" || {
            printf 'legacy Windows stop-only seed matching omits invariant: %s\n' "$literal"
            return 1
        }
    done
    for literal in \
        'secret_file::secure_private_directory' \
        'secret_file::open_private_owned_migration' \
        'secret_file::create_new_private' \
        'Zeroizing::new(serde_json::to_vec_pretty(self)'
    do
        grep -Fq -- "$literal" "$DESKTOP_STORE" || {
            printf 'desktop recovery store bypasses private owner/DACL boundary: %s\n' "$literal"
            return 1
        }
    done

    assert_file_contains "$CLI_KEYGEN" 'run_legacy_seed_file' \
        'CLI lacks the protected file-only legacy identity converter' || return 1
    assert_file_contains "$CLI_KEYGEN" 'secret_file::open_private' \
        'legacy converter bypasses the no-follow private-file reader' || return 1
    assert_file_contains "$CLI_KEYGEN" 'fs::hard_link\(&sidecar, path\)' \
        'CLI keyfile output is not atomic no-replace' || return 1

    assert_file_contains "$INSTALLER" '--validator-key-file "[$]KEY_FILE"' \
        'headless runner does not use the persistent keyfile' || return 1
    assert_file_not_contains "$INSTALLER" \
        'ARC_VALIDATOR_SEED=|export ARC_VALIDATOR_SEED|printf .*node[.]env' \
        'headless installer still materializes an active seed environment' || return 1
}

retired_mutation_examples_are_inert_and_feature_gated() {
    local name source
    python3 - "$NODE_MANIFEST" "${RETIRED_EXAMPLES[@]}" <<'PY' || return 1
import pathlib
import sys
import tomllib

manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
targets = {entry["name"]: entry for entry in manifest.get("example", [])}
for name in sys.argv[2:]:
    target = targets.get(name)
    if target is None:
        raise SystemExit(f"retired example lacks an explicit Cargo target: {name}")
    if target.get("required-features") != ["retired-live-examples"]:
        raise SystemExit(f"retired example is not gated from default builds: {name}")
PY

    for name in "${RETIRED_EXAMPLES[@]}"; do
        source="$REPO_ROOT/crates/arc-node/examples/$name.rs"
        assert_file_contains "$source" 'RETIRED:' \
            "$name does not explain its retired boundary" || return 1
        assert_file_contains "$source" 'std::process::exit[(]78[)]' \
            "$name does not fail with EX_CONFIG" || return 1
        assert_file_not_contains "$source" \
            'reqwest|SigningKey|derive_key|KeyPair|https?://|tx/submit|faucet/claim|ARC-chain-validator-keypair-v1' \
            "$name still contains signing material or a network mutation path" || return 1
    done
}

remaining_mutation_example_is_generated_key_and_loopback_only() {
    assert_file_contains "$LOCAL_MUTATOR" 'KeyPair::generate_ed25519[(][)]' \
        'local E2E mutator does not generate a fresh signing key' || return 1
    assert_file_contains "$LOCAL_MUTATOR" 'require_loopback_rpc' \
        'local E2E mutator bypasses the loopback URL guard' || return 1
    assert_file_not_contains "$LOCAL_MUTATOR" \
        '140[.]82[.]|149[.]28[.]|136[.]244[.]|104[.]238[.]|202[.]182[.]|ARC-chain-validator-keypair-v1|derive_key' \
        'local E2E mutator contains a public endpoint or deterministic signer' || return 1
    assert_file_contains "$LOCAL_GUARD" 'address[.]is_loopback[(][)]' \
        'local RPC guard does not use the standard IP loopback predicate' || return 1
    assert_file_not_contains "$LOCAL_GUARD" \
        'ALLOW|UNSAFE|OVERRIDE|PUBLIC' \
        'local RPC guard contains an override vocabulary' || return 1
}

deterministic_benchmark_signers_are_nondefault_and_local_only() {
    python3 - "$CRYPTO_MANIFEST" "$STATE_MANIFEST" "$NODE_MANIFEST" "$BENCH_MANIFEST" <<'PY' || return 1
import pathlib
import sys
import tomllib

crypto, state, node, bench = [tomllib.loads(pathlib.Path(path).read_text()) for path in sys.argv[1:]]
for name, manifest in [("arc-crypto", crypto), ("arc-state", state), ("arc-node", node), ("arc-bench", bench)]:
    features = manifest.get("features", {})
    if features.get("default") != []:
        raise SystemExit(f"{name} default feature set is not empty")
    if "benchmark-tools" not in features:
        raise SystemExit(f"{name} lacks the nondefault benchmark-tools feature")

if "arc-crypto/benchmark-tools" not in state["features"]["benchmark-tools"]:
    raise SystemExit("arc-state benchmark-tools does not propagate to arc-crypto")
if set(node["features"]["benchmark-tools"]) != {
    "arc-crypto/benchmark-tools",
    "arc-state/benchmark-tools",
}:
    raise SystemExit("arc-node benchmark-tools propagation is incomplete")

targets = {entry["name"]: entry for entry in bench.get("bin", [])}
if targets["arc-bench-multinode"].get("required-features") != ["benchmark-tools"]:
    raise SystemExit("deterministic multi-node benchmark is present in default builds")
PY

    assert_file_contains "$CRYPTO_SIGNATURE" '#\[cfg\(feature = "benchmark-tools"\)\]' \
        'deterministic crypto signer is not feature-gated' || return 1
    assert_file_contains "$CRYPTO_LIB" 'pub use signature::\{benchmark_address, benchmark_keypair\}' \
        'gated benchmark signers are not isolated from default crypto exports' || return 1
    assert_file_contains "$STATE_LIB" 'Deterministic transaction reconstruction is deliberately unavailable' \
        'default state build lacks an inert deterministic-reconstruction boundary' || return 1
    assert_file_contains "$NODE_BENCHMARK" 'cfg\(feature = "benchmark-tools"\)' \
        'node benchmark signing pool is not feature-gated' || return 1

    for literal in \
        'fn default_cli_omits_benchmark_mutation_mode()' \
        'rpc_addr.parse::<SocketAddr>()' \
        'rpc_socket.ip().is_loopback()' \
        'peer_socket.ip().is_loopback()' \
        'cli.insecure_dev_validator_seed && cli.genesis.is_none() && stake > 0' \
        'community_rpc_urls.is_empty()' \
        'if cli.benchmark && (cli.community || cli.community_mode)' \
        'fn p2p_listen_ip(' \
        'if p2p_port == 0 || benchmark_mode || insecure_dev_validator_seed' \
        'let listen_ip = p2p_listen_ip(' \
        'std::net::Ipv4Addr::LOCALHOST'
    do
        grep -Fq -- "$literal" "$NODE_MAIN" || {
            printf 'node benchmark runtime omits fail-closed invariant: %s\n' "$literal"
            return 1
        }
    done
    assert_file_not_contains "$NODE_MAIN" \
        'allow-public-benchmark|allow-remote-benchmark|unsafe-benchmark-network' \
        'node benchmark runtime contains a public-network override' || return 1

    assert_file_contains "$BENCH_EPHEMERAL_KEYS" 'KeyPair::generate_ed25519[(][)]' \
        'single-process benchmarks do not use OS-generated ephemeral identities' || return 1
    for source in production_bench parallel_bench soak_bench mixed_bench propose_verify_bench multinode_bench; do
        assert_file_not_contains "$REPO_ROOT/crates/arc-bench/src/$source.rs" \
            'benchmark_keypair|benchmark_address|SigningKey::from_bytes|ARC-chain-validator-keypair-v1' \
            "$source retains a predictable signer" || return 1
    done
    assert_file_contains "$REPO_ROOT/crates/arc-bench/src/multinode_bench.rs" \
        'require_loopback_benchmark_network' \
        'feature-gated multi-node benchmark lacks its loopback runtime guard' || return 1
    assert_file_contains "$BENCH_README" \
        'default .arc-node. binary has no .--benchmark. option' \
        'benchmark feature-gating tradeoff is undocumented' || return 1
}

run_test 'active benchmark never serves or advertises network-to-shell bootstrap' \
    benchmark_never_serves_or_advertises_remote_shell_bootstrap
run_test 'paper reproduction fetches one exact source revision with a pinned preinstalled toolchain' \
    paper_reproduction_is_source_and_toolchain_pinned
run_test 'documentation contains no mutable executable or model bootstrap' \
    documentation_has_no_mutable_executable_bootstrap
run_test 'Cargo git dependencies are pinned by exact immutable revision' \
    cargo_git_dependencies_use_exact_revisions
run_test 'Python isolated-build backend and dependencies are exactly pinned' \
    python_build_backend_is_exactly_pinned
run_test 'production node, desktop, and installer identities are persistent file-only' \
    persistent_identity_is_file_only_and_no_replace
run_test 'historical public mutation examples are inert and absent from default builds' \
    retired_mutation_examples_are_inert_and_feature_gated
run_test 'remaining mutating example generates its key and accepts only loopback RPC' \
    remaining_mutation_example_is_generated_key_and_loopback_only
run_test 'deterministic benchmark signers are nondefault and numeric-loopback-only' \
    deterministic_benchmark_signers_are_nondefault_and_local_only

finish_tests

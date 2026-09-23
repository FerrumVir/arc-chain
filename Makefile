.PHONY: build test test-fast test-integration node join inference inference-node \
        explorer faucet bench stats health eval-perplexity clean \
        fmt fmt-check lint audit desktop-test test-scripts ci help

# Default target: show what's available rather than silently building.
help:
	@echo "ARC Chain make targets"
	@echo ""
	@echo "  Verification (mirror CI):"
	@echo "    make ci               Run the full blocking local/CI gate"
	@echo "    make fmt              Format the workspace in place"
	@echo "    make fmt-check        Fail if anything is unformatted (what CI runs)"
	@echo "    make lint             clippy --all-targets -D warnings"
	@echo "    make test             Unit tests (the blocking CI gate)"
	@echo "    make test-integration Integration + doc tests (not run by --lib)"
	@echo "    make audit            cargo deny check (advisories/licenses/bans/sources)"
	@echo "    make desktop-test     Desktop typecheck + stable Playwright/Tauri tests"
	@echo ""
	@echo "  Build / run:"
	@echo "    make build node join inference inference-node explorer faucet bench"
	@echo "    make stats health eval-perplexity clean"

# Build everything
build:
	cargo build --release

# ---------------------------------------------------------------------------
# Verification targets. These mirror .github/workflows/ci.yml so that a green
# `make ci` locally means the same thing as a green CI run. If you change one,
# change the other.
#
# `--locked` everywhere: it makes a stale or missing Cargo.lock a loud failure
# instead of a silent re-resolve, which is the whole point of tracking the
# lockfile.
# ---------------------------------------------------------------------------

# Format in place. Run this once, as its own commit, before making the CI fmt
# job blocking - the first diff is large and purely mechanical.
fmt:
	./scripts/rustfmt-workspace.sh

# What CI runs.
fmt-check:
	./scripts/rustfmt-workspace.sh --check

lint:
	cargo clippy --workspace --all-targets --locked -- -D warnings

# The blocking CI gate: library unit tests only. Fast.
#
# The second line exists because `--lib` with default features silently SKIPS
# two conformance tests: the canonical GGUF loader's preparation vectors and
# the loader's refusal of a model missing a norm weight both sit behind the
# `candle` feature. Without this, the only check that the Rust loader prepares
# a GGUF exactly as the independent Python reference does never runs in CI, and
# a drift in either implementation would go unnoticed until someone ran it by
# hand. It costs a few seconds; the crate is already compiled by then.
test:
	cargo test --workspace --lib --locked
	cargo test -p arc-inference --lib --features candle --locked

# Alias, for when it's ambiguous which one you meant.
test-fast: test

# Everything --lib excludes: the multi-node consensus suite, the on-chain
# inference e2e test, and all doc tests. Single-threaded because
# crates/arc-node/tests/multi_node.rs binds real UDP sockets and its tests
# race each other for ports otherwise.
test-integration:
	cargo test --workspace --test '*' --locked -- --test-threads=1
	cargo test --workspace --doc --locked

# Supply chain: advisories, licence policy, duplicate versions, git sources.
# Needs `cargo install cargo-deny`. Config is deny.toml at the repo root.
audit:
	cargo deny check

# Desktop typecheck + deterministic Playwright gate + stable Tauri tests. The
# screenshot gallery and ambient live-node suite remain manual-only.
desktop-test:
	cd desktop && npm ci && npx tsc --noEmit && npx playwright install chromium && CI=true npx playwright test --config playwright.gate.config.ts
	cargo +stable test --manifest-path desktop/src-tauri/Cargo.toml --all-targets --locked

# The non-Rust suites that .github/workflows/ci.yml's `script-suites` job
# runs: arc_conformance/arc_soak/arc_ops/scripts-release Python tests, the
# explorer local-dev contract, and desktop's pure-logic Playwright specs.
# Needs network the first time (pip install, npm ci). If you change one of
# the three commands here, change scripts/ci/run-script-suites.sh and the
# `script-suites` job to match.
test-scripts:
	python3 -m pip install --quiet "blake3>=1.0" "numpy>=1.26"
	bash scripts/ci/run-script-suites.sh
	node explorer/test-localdev-native.mjs
	cd desktop && npm ci && npm run test:unit

# The single source of truth for every required local gate.
ci:
	./scripts/ci_check.sh --full

# ---------------------------------------------------------------------------
# Existing targets, unchanged.
# ---------------------------------------------------------------------------

# Start a local node
node:
	cargo run --release -p arc-node

# Join the live testnet
join:
	./scripts/join-testnet.sh

# Join as a stake-zero inference worker with an operator-verified local model.
inference:
	@test -n "$$ARC_MODEL_PATH" || { echo "Set ARC_MODEL_PATH=/absolute/path/to/model.gguf" >&2; exit 2; }
	./scripts/join-inference.sh --model "$$ARC_MODEL_PATH"

# Alias for the same stake-zero worker flow. Work/rewards are not guaranteed.
inference-node:
	@test -n "$$ARC_MODEL_PATH" || { echo "Set ARC_MODEL_PATH=/absolute/path/to/model.gguf" >&2; exit 2; }
	./scripts/join-inference.sh --model "$$ARC_MODEL_PATH"

# Run the block explorer
explorer:
	@if [ "$$(uname)" = "Darwin" ]; then open explorer/index-live.html; \
	elif command -v xdg-open >/dev/null; then xdg-open explorer/index-live.html; \
	else echo "Open explorer/index-live.html in your browser"; fi

# Run the testnet faucet
faucet:
	cd faucet && cargo run --release

# Run benchmarks
bench:
	cargo run --release --bin arc-bench-multinode

# Check chain stats on live testnet
stats:
	@curl -s http://140.82.16.112:9090/stats | python3 -m json.tool

# Check live node health
health:
	@curl -s http://140.82.16.112:9090/health | python3 -m json.tool

# Run perplexity evaluation
eval-perplexity:
	./scripts/eval-perplexity.sh

# Clean build artifacts
clean:
	cargo clean

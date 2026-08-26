.PHONY: build test test-fast test-integration node join inference inference-node \
        explorer faucet bench stats health eval-perplexity clean \
        fmt fmt-check lint audit desktop-test ci help

# Default target: show what's available rather than silently building.
help:
	@echo "ARC Chain make targets"
	@echo ""
	@echo "  Verification (mirror CI):"
	@echo "    make ci               Run the full local gate (fmt-check, lint, test, audit)"
	@echo "    make fmt              Format the workspace in place"
	@echo "    make fmt-check        Fail if anything is unformatted (what CI runs)"
	@echo "    make lint             clippy --all-targets -D warnings"
	@echo "    make test             Unit tests (the blocking CI gate)"
	@echo "    make test-integration Integration + doc tests (not run by --lib)"
	@echo "    make audit            cargo deny check (advisories/licenses/bans/sources)"
	@echo "    make desktop-test     Desktop typecheck + Playwright suite"
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
test:
	cargo test --workspace --lib --locked

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

# Desktop typecheck + the 15-spec Playwright suite (runs against the mock
# Tauri backend, so no arc-node binary is needed).
desktop-test:
	cd desktop && npm ci && npx tsc --noEmit && npx playwright install --with-deps chromium && npx playwright test

# The local aggregate. Deliberately ordered cheapest-first so it fails fast.
# Does not include test-integration or desktop-test: both are slow, and both
# are non-blocking in CI today. Run them explicitly.
ci: fmt-check lint test audit
	@echo "ALL LOCAL CHECKS PASSED"

# ---------------------------------------------------------------------------
# Existing targets, unchanged.
# ---------------------------------------------------------------------------

# Start a local node
node:
	cargo run --release -p arc-node

# Join the live testnet
join:
	./scripts/join-testnet.sh

# Join testnet with inference enabled (downloads model)
inference:
	./scripts/join-testnet.sh --with-inference

# Join as an inference node (GPU recommended, earn ARC)
inference-node:
	./scripts/join-inference.sh

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

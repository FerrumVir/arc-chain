.PHONY: build test test-fast test-integration node join inference inference-node \
        explorer faucet bench stats health eval-perplexity clean \
        fmt fmt-check lint audit desktop-test test-scripts ci help

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

build:
	cargo build --release

fmt:
	./scripts/rustfmt-workspace.sh

fmt-check:
	./scripts/rustfmt-workspace.sh --check

lint:
	cargo clippy --workspace --all-targets --locked -- -D warnings

test:
	cargo test --workspace --lib --locked
	cargo test -p arc-inference --lib --features candle --locked

test-fast: test

test-integration:
	cargo test --workspace --test '*' --locked -- --test-threads=1
	cargo test --workspace --doc --locked

audit:
	cargo deny check

desktop-test:
	cd desktop && npm ci && npx tsc --noEmit && npx playwright install chromium && CI=true npx playwright test --config playwright.gate.config.ts
	cargo +stable test --manifest-path desktop/src-tauri/Cargo.toml --all-targets --locked

test-scripts:
	python3 -m pip install --quiet "blake3>=1.0" "numpy>=1.26"
	bash scripts/ci/run-script-suites.sh
	node explorer/test-localdev-native.mjs
	cd desktop && npm ci && npm run test:unit

ci:
	./scripts/ci_check.sh --full

node:
	cargo run --release -p arc-node

join:
	./scripts/join-testnet.sh

inference:
	@test -n "$$ARC_MODEL_PATH" || { echo "Set ARC_MODEL_PATH=/absolute/path/to/model.gguf" >&2; exit 2; }
	./scripts/join-inference.sh --model "$$ARC_MODEL_PATH"

inference-node:
	@test -n "$$ARC_MODEL_PATH" || { echo "Set ARC_MODEL_PATH=/absolute/path/to/model.gguf" >&2; exit 2; }
	./scripts/join-inference.sh --model "$$ARC_MODEL_PATH"

explorer:
	@if [ "$$(uname)" = "Darwin" ]; then open explorer/index-live.html; \
	elif command -v xdg-open >/dev/null; then xdg-open explorer/index-live.html; \
	else echo "Open explorer/index-live.html in your browser"; fi

faucet:
	cd faucet && cargo run --release

bench:
	cargo run --release --bin arc-bench-multinode

stats:
	@curl -s http://140.82.16.112:9090/stats | python3 -m json.tool

health:
	@curl -s http://140.82.16.112:9090/health | python3 -m json.tool

eval-perplexity:
	./scripts/eval-perplexity.sh

clean:
	cargo clean

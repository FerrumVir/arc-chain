.PHONY: build test node join inference explorer faucet bench clean

# Build everything
build:
	cargo build --release

# Run all tests
test:
	cargo test --workspace --lib

# Start a local node
node:
	cargo run --release -p arc-node

# Join the live testnet
join:
	./scripts/join-testnet.sh

# Join testnet with inference enabled (downloads model)
inference:
	./scripts/join-testnet.sh --with-inference

# Run the block explorer
explorer:
	open explorer/index-live.html

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

# Clean build artifacts
clean:
	cargo clean

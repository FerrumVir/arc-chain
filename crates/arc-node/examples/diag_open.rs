//! Retired public-testnet escrow-open diagnostic.

fn main() {
    eprintln!(
        "RETIRED: diag_open cannot submit signed transactions; use an isolated loopback integration test with generated keys."
    );
    std::process::exit(78);
}

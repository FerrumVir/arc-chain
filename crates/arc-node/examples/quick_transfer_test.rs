//! Retired public-testnet transfer diagnostic.

fn main() {
    eprintln!(
        "RETIRED: quick_transfer_test cannot submit with a deterministic signer; use a loopback integration test with generated keys."
    );
    std::process::exit(78);
}

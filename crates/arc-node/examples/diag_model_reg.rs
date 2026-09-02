//! Retired public-testnet model-registration diagnostic.

fn main() {
    eprintln!(
        "RETIRED: diag_model_reg cannot submit signed transactions; use an isolated loopback integration test with generated keys."
    );
    std::process::exit(78);
}

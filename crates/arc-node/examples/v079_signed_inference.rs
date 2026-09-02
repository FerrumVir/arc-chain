//! Retired v0.7.9 public-testnet signed-inference smoke driver.

fn main() {
    eprintln!(
        "RETIRED: v079_signed_inference cannot submit to legacy public endpoints; use a loopback integration test with generated keys."
    );
    std::process::exit(78);
}

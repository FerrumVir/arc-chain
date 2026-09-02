//! Retired public-testnet paid-inference mutation driver.

fn main() {
    eprintln!(
        "RETIRED: live_paid_inference cannot mutate a public network; use generated keys in an isolated loopback integration test."
    );
    std::process::exit(78);
}

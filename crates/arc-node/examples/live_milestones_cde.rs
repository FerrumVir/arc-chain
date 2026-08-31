//! Retired public-testnet milestone mutation driver.

fn main() {
    eprintln!(
        "RETIRED: live_milestones_cde cannot mutate a public network; use generated keys in an isolated loopback integration test."
    );
    std::process::exit(78);
}

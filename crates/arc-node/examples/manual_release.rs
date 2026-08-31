//! Retired one-off public-testnet escrow-release mutator.

fn main() {
    eprintln!(
        "RETIRED: manual_release cannot replay an embedded signer or submit a release transaction."
    );
    std::process::exit(78);
}

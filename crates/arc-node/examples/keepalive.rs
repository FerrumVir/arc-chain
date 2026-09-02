//! Retired public-testnet keepalive mutator.

fn main() {
    eprintln!(
        "RETIRED: keepalive cannot mutate a network; block production must never depend on a deterministic signing bot."
    );
    std::process::exit(78);
}

pub mod account;
pub mod account_abstraction;
pub mod ai_native;
pub mod block;
pub mod bridge;
pub mod defi;
pub mod devtools;
pub mod economics;
pub mod governance;
pub mod identity;
pub mod intent;
pub mod multisig;
pub mod proof_market;
pub mod sdk;
pub mod session_keys;
pub mod social_recovery;
pub mod transaction;
pub mod wallet;

pub use account::*;
pub use block::*;
pub use identity::*;
pub use transaction::*;

/// Return the minimum voting power that is strictly greater than two thirds
/// of `total_power`.
///
/// This subtraction form is equivalent to `floor(2 * total_power / 3) + 1`
/// but cannot overflow at `u64::MAX`. A zero-power set deliberately requires
/// one unit so it cannot authorize anything.
pub const fn strict_supermajority_threshold(total_power: u64) -> u64 {
    if total_power == 0 {
        1
    } else {
        total_power - (total_power - 1) / 3
    }
}

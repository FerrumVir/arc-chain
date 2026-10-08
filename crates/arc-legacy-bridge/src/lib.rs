//! ARC legacy bridge.
//!
//! Released v0.7 updaters (the desktop app's `ensure_binary`, the headless
//! `arc-auto-update.sh` timer, and the older `scripts/auto-update.sh` daemon)
//! only ever replace `bin/arc-node` with GitHub's `latest` asset and restart it
//! with their unchanged v0.7 command line. They verify nothing. This crate is
//! the program they receive under the legacy asset names in the v0.7.12
//! bridge release. It never runs v0.7 code and never touches v0.7 state.
//!
//! On every start it:
//!
//! 1. recognizes the exact v0.7 desktop or headless invocation and refuses
//!    anything else, including any explicit non-zero `--stake`;
//! 2. records a stat-only manifest of the v0.7 data directory and never opens
//!    a v0.7 file for writing, renaming, or deletion;
//! 3. fetches the v0.8 release pinned at compile time by exact tag, commit,
//!    size, and SHA-256, after verifying the owner-signed `SHA256SUMS` with the
//!    same Ed25519 release key and namespace that `install.sh` uses;
//! 4. creates a fresh identity and a fresh data directory beside (never
//!    inside) the v0.7 state, and starts the pinned node with `--stake 0`;
//! 5. keeps compute off unless the operator recorded consent and a verified
//!    model, and hides the node's model auto-discovery paths while it is off.
//!
//! The v0.7 seed passed on the legacy command line is ignored: it is never
//! stored, logged, transformed, or forwarded.

pub mod archive;
pub mod argv;
pub mod consent;
pub mod exit;
pub mod fetch;
pub mod hashing;
pub mod launch;
pub mod layout;
pub mod logging;
pub mod manifest;
pub mod pins;
pub mod release;
pub mod run;
pub mod sshsig;
pub mod state;

pub use run::main_entry;

//! Exit codes (sysexits.h values) and error classification.
//!
//! Supervisors restart the bridge whatever it returns, so the codes exist for
//! people and tests: a refusal means "this machine will never be bridged as
//! is", a temporary failure means "try again later", and unavailable means
//! the network or GitHub could not deliver the pinned release.

use std::fmt;

pub const EX_OK: i32 = 0;
pub const EX_USAGE: i32 = 64;
pub const EX_UNAVAILABLE: i32 = 69;
pub const EX_SOFTWARE: i32 = 70;
pub const EX_TEMPFAIL: i32 = 75;
pub const EX_CONFIG: i32 = 78;

#[derive(Debug)]
pub struct Classified {
    pub code: i32,
    pub message: String,
}

impl fmt::Display for Classified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Classified {}

fn classified(code: i32, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Classified {
        code,
        message: message.into(),
    })
}

/// The invocation or machine is not something the bridge will ever touch.
pub fn refused(message: impl Into<String>) -> anyhow::Error {
    classified(EX_CONFIG, message)
}

/// The command line is not a recognized v0.7 invocation.
pub fn usage(message: impl Into<String>) -> anyhow::Error {
    classified(EX_USAGE, message)
}

/// Safe to retry later without any change on this machine.
pub fn temporary(message: impl Into<String>) -> anyhow::Error {
    classified(EX_TEMPFAIL, message)
}

/// The pinned release could not be fetched.
pub fn unavailable(message: impl Into<String>) -> anyhow::Error {
    classified(EX_UNAVAILABLE, message)
}

pub fn code_for(error: &anyhow::Error) -> i32 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Classified>())
        .map_or(EX_SOFTWARE, |classified| classified.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn classification_survives_context() {
        let error = Err::<(), _>(refused("no"))
            .context("while bridging")
            .unwrap_err();
        assert_eq!(code_for(&error), EX_CONFIG);
        assert_eq!(code_for(&temporary("later")), EX_TEMPFAIL);
        assert_eq!(code_for(&anyhow::anyhow!("plain")), EX_SOFTWARE);
    }
}

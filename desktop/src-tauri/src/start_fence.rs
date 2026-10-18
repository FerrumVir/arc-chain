//! The native fence in front of every entrypoint that can start, restart,
//! update or replace the managed arc-node.
//!
//! At launch the desktop first reconciles the node an earlier desktop process
//! left behind. It drains any node still running and, when an earlier node
//! exited without acknowledging its final WAL barrier (a crash, a forced quit,
//! or the computer restarting or losing power while the node ran), it runs one
//! quarantined recovery launch of that exact node so the node can replay its
//! WAL and shut down cleanly. WebView Start, Retry, Restart, Ensure binary and
//! update preparation wait until that is done. The recovery launch is the step
//! they are waiting for, so it alone may pass the launch-time fence.
//!
//! Through v0.8.12 this state was a plain message, checked the same way for
//! every caller. The recovery launch therefore refused itself with the
//! launch-time "in progress" placeholder; startup recorded that refusal as a
//! lasting failure, the shutdown receipt was never cleared, and every later
//! launch failed the same way. Users saw "managed-node durability recovery
//! failed: ARC refused to start the node because chain-data migration is not
//! safely resolved: managed-node startup reconciliation is still in progress".
//!
//! This module depends on `std` only, so its tests also run standalone with
//! `rustc --edition 2021 --test src/start_fence.rs`.

/// Why native node-start entrypoints are closed right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartFence {
    /// Launch-time reconciliation still owns the managed-node lifecycle. Only
    /// its own recovery launch may pass.
    StartupReconciliation,
    /// A refusal that holds until ARC restarts: the legacy-data migration is
    /// not durable, the shutdown receipt cannot be validated, or the startup
    /// recovery launch failed. Nothing passes, the recovery launch included.
    Refused(String),
}

/// Who is asking to pass the fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartAccess {
    /// WebView Start, Retry, Restart, Ensure binary, peer reset and update
    /// preparation, and auto-start after reconciliation.
    Ordinary,
    /// The launch-time reconciliation's own quarantined recovery launch
    /// (`commands::recover_managed_shutdown_inner`). Nothing else uses it.
    StartupRecovery,
}

/// What an ordinary caller sees while launch-time reconciliation runs. It is
/// transient, so it does not tell the user to repair anything.
pub const STARTUP_RECONCILIATION_IN_PROGRESS: &str =
    "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds";

/// The fence before launch-time reconciliation begins. A migration failure
/// found during setup is already a refusal; otherwise reconciliation holds
/// the fence until it returns.
pub fn at_launch(migration_failure: Option<String>) -> StartFence {
    match migration_failure {
        Some(reason) => StartFence::Refused(reason),
        None => StartFence::StartupReconciliation,
    }
}

/// The fence once launch-time reconciliation has returned. Success opens it
/// and a failure becomes an explicit refusal carrying its reason, so the
/// "in progress" state never outlives the reconciliation that set it.
pub fn after_startup_reconciliation(outcome: Result<(), String>) -> Option<StartFence> {
    match outcome {
        Ok(()) => None,
        Err(reason) => Some(StartFence::Refused(reason)),
    }
}

/// Decide whether `access` may pass `fence`.
pub fn check(fence: Option<&StartFence>, access: StartAccess) -> Result<(), String> {
    match (fence, access) {
        (None, _) => Ok(()),
        (Some(StartFence::StartupReconciliation), StartAccess::StartupRecovery) => Ok(()),
        (Some(StartFence::StartupReconciliation), StartAccess::Ordinary) => {
            Err(format!("{STARTUP_RECONCILIATION_IN_PROGRESS}."))
        }
        (Some(StartFence::Refused(reason)), _) => Err(format!(
            "ARC refused to start the node because chain-data migration is not safely resolved: {reason}. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v0.8.12 field failure. Startup recovery runs while the launch-time
    /// fence is up; it must pass it instead of refusing itself.
    #[test]
    fn startup_recovery_passes_the_launch_fence_it_serves() {
        let fence = at_launch(None);
        assert_eq!(fence, StartFence::StartupReconciliation);
        assert_eq!(check(Some(&fence), StartAccess::StartupRecovery), Ok(()));
    }

    #[test]
    fn ordinary_callers_wait_for_startup_reconciliation() {
        let error = check(
            Some(&StartFence::StartupReconciliation),
            StartAccess::Ordinary,
        )
        .expect_err("Start, Retry and update preparation must wait for reconciliation");
        assert!(error.contains("startup reconciliation is still in progress"));
        assert!(error.contains("Try again in a few seconds"));
        // A transient wait is not a migration fault: no repair instructions.
        assert!(!error.contains("not safely resolved"));
        assert!(!error.contains("preserved legacy directory"));
    }

    #[test]
    fn refusals_block_every_caller_including_recovery() {
        let fence = StartFence::Refused("legacy data directory is a symbolic link".into());
        for access in [StartAccess::Ordinary, StartAccess::StartupRecovery] {
            let error = check(Some(&fence), access)
                .expect_err("a migration refusal must block every start path");
            assert!(error.contains("refused to start"));
            assert!(error.contains("migration is not safely resolved"));
            assert!(error.contains("legacy data directory is a symbolic link"));
            assert!(error.contains("do not point v0.8 at the preserved legacy directory"));
        }
    }

    #[test]
    fn an_open_fence_admits_every_caller() {
        assert_eq!(check(None, StartAccess::Ordinary), Ok(()));
        assert_eq!(check(None, StartAccess::StartupRecovery), Ok(()));
    }

    #[test]
    fn a_migration_failure_found_at_setup_is_refused_from_the_start() {
        let fence = at_launch(Some(
            "the protected protocol-v3 data pointer could not be persisted: disk full".into(),
        ));
        assert!(matches!(fence, StartFence::Refused(_)));
        assert!(check(Some(&fence), StartAccess::StartupRecovery).is_err());
    }

    /// The "in progress" state must never be left behind, whichever way
    /// reconciliation ends.
    #[test]
    fn reconciliation_never_leaves_the_in_progress_fence_behind() {
        assert_eq!(after_startup_reconciliation(Ok(())), None);
        let reason = "managed-node durability recovery failed: arc-node exited during WAL replay";
        assert_eq!(
            after_startup_reconciliation(Err(reason.to_string())),
            Some(StartFence::Refused(reason.to_string()))
        );
    }

    /// Start succeeds once reconciliation completes, and a failed recovery
    /// is reported as itself, not as a nested copy of the launch fence.
    #[test]
    fn launch_sequence_opens_after_recovery_and_reports_failures_plainly() {
        let mut fence = Some(at_launch(None));
        assert!(check(fence.as_ref(), StartAccess::Ordinary).is_err());
        assert!(check(fence.as_ref(), StartAccess::StartupRecovery).is_ok());

        fence = after_startup_reconciliation(Ok(()));
        assert_eq!(check(fence.as_ref(), StartAccess::Ordinary), Ok(()));

        fence = after_startup_reconciliation(Err(
            "managed-node durability recovery failed: recovery node exited before its WAL barrier"
                .into(),
        ));
        let error = check(fence.as_ref(), StartAccess::Ordinary).unwrap_err();
        assert!(error.contains("recovery node exited before its WAL barrier"));
        assert!(!error.contains(STARTUP_RECONCILIATION_IN_PROGRESS));
        assert_eq!(error.matches("ARC refused to start the node").count(), 1);
    }
}

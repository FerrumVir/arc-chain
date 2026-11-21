#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartFence {
    StartupReconciliation,
    Refused(String),
    MigrationRefused(String),
    OtherDesktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartAccess {
    Ordinary,
    StartupRecovery,
}

pub const STARTUP_RECONCILIATION_IN_PROGRESS: &str =
    "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds";

pub const ALREADY_OPEN: &str = "ARC is already open on this computer.";

const REFUSAL_PREFIX: &str =
    "ARC refused to start the node because chain-data migration is not safely resolved: ";
const REFUSAL_ADVICE: &str = "Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppUpdateBoundary {
    Ordinary,
    Fenced,
}

pub fn at_launch(migration_failure: Option<String>) -> StartFence {
    match migration_failure {
        Some(reason) => StartFence::MigrationRefused(reason),
        None => StartFence::StartupReconciliation,
    }
}

pub fn after_startup_reconciliation(outcome: Result<(), String>) -> Option<StartFence> {
    match outcome {
        Ok(()) => None,
        Err(reason) if reason == ALREADY_OPEN => Some(StartFence::OtherDesktop),
        Err(reason) => Some(StartFence::Refused(reason)),
    }
}

pub fn check(fence: Option<&StartFence>, access: StartAccess) -> Result<(), String> {
    match (fence, access) {
        (None, _) => Ok(()),
        (Some(StartFence::StartupReconciliation), StartAccess::StartupRecovery) => Ok(()),
        (Some(StartFence::StartupReconciliation), StartAccess::Ordinary) => {
            Err(format!("{STARTUP_RECONCILIATION_IN_PROGRESS}."))
        }
        (Some(StartFence::MigrationRefused(reason)), _) => Err(refusal_message(reason)),
        (Some(StartFence::OtherDesktop), _) => Err(ALREADY_OPEN.into()),
        (Some(StartFence::Refused(reason)), _) => {
            Err(format!("ARC refused to start the node: {reason}"))
        }
    }
}

pub fn check_settings_write(fence: Option<&StartFence>, closing: bool) -> Result<(), String> {
    if closing {
        return Err("ARC is closing".into());
    }
    if matches!(fence, Some(StartFence::OtherDesktop)) {
        return Err(ALREADY_OPEN.into());
    }
    Ok(())
}

fn refusal_message(reason: &str) -> String {
    if reason.contains(REFUSAL_PREFIX) {
        reason.to_owned()
    } else {
        format!("{REFUSAL_PREFIX}{reason}. {REFUSAL_ADVICE}")
    }
}

pub fn app_update_boundary(fence: Option<&StartFence>) -> Result<AppUpdateBoundary, String> {
    match fence {
        None => Ok(AppUpdateBoundary::Ordinary),
        Some(StartFence::StartupReconciliation) => {
            Err(format!("{STARTUP_RECONCILIATION_IN_PROGRESS}."))
        }
        Some(StartFence::OtherDesktop) => Err(ALREADY_OPEN.into()),
        Some(StartFence::Refused(_) | StartFence::MigrationRefused(_)) => {
            Ok(AppUpdateBoundary::Fenced)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_write_is_independent_of_start_reconciliation() {
        for fence in [
            None,
            Some(StartFence::StartupReconciliation),
            Some(StartFence::Refused("recovery".into())),
            Some(StartFence::MigrationRefused("migration".into())),
        ] {
            assert!(check_settings_write(fence.as_ref(), false).is_ok());
            assert_eq!(
                check_settings_write(fence.as_ref(), true).unwrap_err(),
                "ARC is closing"
            );
        }
        assert_eq!(
            check_settings_write(Some(&StartFence::OtherDesktop), false).unwrap_err(),
            ALREADY_OPEN
        );
    }

    #[test]
    fn contention_and_recovery_are_not_migration_failures() {
        let fence = after_startup_reconciliation(Err(ALREADY_OPEN.into())).unwrap();
        assert_eq!(fence, StartFence::OtherDesktop);
        assert_eq!(
            check(Some(&fence), StartAccess::Ordinary),
            Err(ALREADY_OPEN.into())
        );
        assert!(app_update_boundary(Some(&fence)).is_err());
        let fence = after_startup_reconciliation(Err("shutdown receipt invalid".into())).unwrap();
        let message = check(Some(&fence), StartAccess::Ordinary).unwrap_err();
        assert!(!message.contains("migration"));
        assert!(!message.contains("preserved legacy"));
    }

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
        assert!(!error.contains("not safely resolved"));
        assert!(!error.contains("preserved legacy directory"));
    }

    #[test]
    fn refusals_block_every_caller_including_recovery() {
        let fence = StartFence::MigrationRefused("legacy data directory is a symbolic link".into());
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
        assert!(matches!(fence, StartFence::MigrationRefused(_)));
        assert!(check(Some(&fence), StartAccess::StartupRecovery).is_err());
    }

    #[test]
    fn reconciliation_never_leaves_the_in_progress_fence_behind() {
        assert_eq!(after_startup_reconciliation(Ok(())), None);
        let reason = "managed-node durability recovery failed: arc-node exited during WAL replay";
        assert_eq!(
            after_startup_reconciliation(Err(reason.to_string())),
            Some(StartFence::Refused(reason.to_string()))
        );
    }

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

    #[test]
    fn a_refusal_lets_the_app_update_through_and_nothing_else() {
        for reason in [
            "managed-node durability recovery failed: recovery node exited before its WAL barrier",
            "legacy-data migration preflight failed: legacy data directory is a symbolic link",
            "managed-node startup reconciliation failed: the private managed-node shutdown receipt cannot be validated",
        ] {
            let fence = StartFence::Refused(reason.into());
            assert_eq!(
                app_update_boundary(Some(&fence)),
                Ok(AppUpdateBoundary::Fenced)
            );
            for access in [StartAccess::Ordinary, StartAccess::StartupRecovery] {
                assert!(check(Some(&fence), access).is_err(), "{reason}");
            }
        }
        let fence = at_launch(Some("disk full".into()));
        assert_eq!(
            app_update_boundary(Some(&fence)),
            Ok(AppUpdateBoundary::Fenced)
        );
    }

    #[test]
    fn the_app_update_takes_the_ordinary_boundary_when_the_fence_is_open() {
        assert_eq!(app_update_boundary(None), Ok(AppUpdateBoundary::Ordinary));
        assert_eq!(
            app_update_boundary(after_startup_reconciliation(Ok(())).as_ref()),
            Ok(AppUpdateBoundary::Ordinary)
        );
    }

    #[test]
    fn the_app_update_waits_for_startup_reconciliation() {
        let error = app_update_boundary(Some(&at_launch(None)))
            .expect_err("the update must not interrupt the recovery launch");
        assert!(error.contains("startup reconciliation is still in progress"));
        assert!(!error.contains(REFUSAL_PREFIX));
    }

    #[test]
    fn a_refusal_inside_a_refusal_is_framed_once() {
        let inner = refusal_message("managed-node startup reconciliation is still in progress");
        let fence = StartFence::MigrationRefused(format!(
            "managed-node durability recovery failed: {inner}"
        ));
        let error = check(Some(&fence), StartAccess::Ordinary).unwrap_err();
        assert_eq!(error.matches(REFUSAL_PREFIX).count(), 1);
        assert_eq!(error.matches(REFUSAL_ADVICE).count(), 1);
        assert!(error.contains("managed-node durability recovery failed"));
        assert!(!error.contains(".."));
    }
}

use std::{cmp::Ordering, future::Future};

use crate::{Error, ReleaseVersion, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum UpdatePlan {
    Install,
    UpToDate,
    DowngradeBlocked,
}

/// Decide by SemVer precedence; build metadata does not change precedence.
pub fn plan(current: &str, release: &ReleaseVersion, force: bool) -> Result<UpdatePlan> {
    let current = semver::Version::parse(current.trim_start_matches('v'))?;
    Ok(if force {
        UpdatePlan::Install
    } else {
        match current.cmp_precedence(&release.version) {
            Ordering::Less => UpdatePlan::Install,
            Ordering::Equal => UpdatePlan::UpToDate,
            Ordering::Greater => UpdatePlan::DowngradeBlocked,
        }
    })
}

pub fn update_check_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|s| {
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Drop the operation future on an ordinary termination signal. TempDir and
/// installation guards then clean up. Replacement itself has no await point.
pub async fn cancellable<T>(operation: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::select! {
        result = operation => result,
        signal = termination_signal() => { signal?; Err(Error::Cancelled) }
    }
}

async fn termination_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut hup = signal(SignalKind::hangup())?;
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = hup.recv() => (),
            _ = term.recv() => (),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_decisions_cover_force_downgrade_and_prerelease() {
        for (current, available, force, expected) in [
            ("0.3.9", "0.3.10", false, UpdatePlan::Install),
            ("0.3.4", "0.3.4", false, UpdatePlan::UpToDate),
            ("0.3.4+one", "0.3.4+two", false, UpdatePlan::UpToDate),
            ("0.3.5", "0.3.4", false, UpdatePlan::DowngradeBlocked),
            ("0.3.5", "0.3.4", true, UpdatePlan::Install),
            ("0.3.4", "0.3.4", true, UpdatePlan::Install),
            ("0.3.5-rc.1", "0.3.5", false, UpdatePlan::Install),
            ("0.3.5", "0.3.5-rc.1", false, UpdatePlan::DowngradeBlocked),
        ] {
            assert_eq!(
                plan(current, &ReleaseVersion::parse(available).unwrap(), force).unwrap(),
                expected
            );
        }
        assert!(plan("invalid", &ReleaseVersion::parse("0.3.4").unwrap(), false).is_err());
    }

    #[test]
    fn disable_switch_accepts_only_truthy_values() {
        for value in [None, Some(""), Some("0"), Some("false"), Some("no")] {
            assert!(!update_check_disabled(value));
        }
        for value in ["1", "true", "YES", " on "] {
            assert!(update_check_disabled(Some(value)));
        }
    }
}

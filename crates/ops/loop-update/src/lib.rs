//! Distribution operations. This crate depends on no runtime or surface crate.
//! User configuration and sessions are deliberately outside its ownership.
//!
//! The pieces are:
//!
//! - [`ReleaseClient`] finds, downloads and verifies a release (GitHub, or a
//!   mirror).
//! - [`Installation`] owns the installed `loop` binary and its `.loop-update/`
//!   state folder, and replaces, rolls back or removes the binary.
//! - [`plan`] and the other helpers decide whether to update.

#![deny(missing_docs)]

mod control;
mod install;
mod release;

pub use control::{cancellable, plan, update_check_disabled, UpdatePlan};
pub use install::{default_install_dir, Installation};
pub use release::{is_newer, platform_target, ReleaseClient, ReleaseNotes, ReleaseVersion};

/// Why an update operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A precondition or validation check failed. The message is shown to the
    /// user as written.
    #[error("{0}")]
    Invalid(String),
    /// A file system or process I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A network request failed or returned an error status.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// A release response or a state file was not valid JSON of the expected shape.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A version string was not valid SemVer.
    #[error(transparent)]
    Version(#[from] semver::Error),
    /// A termination signal arrived during the operation. The installed binary
    /// is unchanged.
    #[error("Update cancelled; current binary kept")]
    Cancelled,
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The lowercase hex SHA-256 of the file at `path`, read in a stream.
pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

/// Accept exactly one SHA-256 entry, optionally with the asset name.
pub fn checksum(text: &str, asset: &str) -> Result<String> {
    let text = text.trim_start_matches('\u{feff}').trim();
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.is_empty()
        || fields.len() > 2
        || fields[0].len() != 64
        || !fields[0].bytes().all(|b| b.is_ascii_hexdigit())
        || (fields.len() == 2 && fields[1].trim_start_matches('*') != asset)
    {
        return Err(Error::Invalid("Missing or invalid SHA-256 checksum".into()));
    }
    Ok(fields[0].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_invalid_and_wrong_asset_checksums() {
        let hash = "a".repeat(64);
        assert_eq!(
            checksum(&format!("{hash}  loop.tar.gz\n"), "loop.tar.gz").unwrap(),
            hash
        );
        assert!(checksum("", "loop.tar.gz").is_err());
        assert!(checksum(&"g".repeat(64), "loop.tar.gz").is_err());
        assert!(checksum(&format!("{hash} other.tar.gz"), "loop.tar.gz").is_err());
        assert!(checksum(&format!("{hash}\n{hash}"), "loop.tar.gz").is_err());
    }
}

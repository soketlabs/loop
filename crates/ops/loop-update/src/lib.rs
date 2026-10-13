//! Distribution operations. This crate depends on no runtime or surface crate.
//! User configuration and sessions are deliberately outside its ownership.

mod install;
mod release;

pub use install::{default_install_dir, Installation};
pub use release::{is_newer, platform_target, ReleaseClient, ReleaseVersion};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Version(#[from] semver::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, Error>;

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

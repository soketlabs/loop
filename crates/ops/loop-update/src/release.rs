use std::{
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use reqwest::Url;
use semver::Version;
use serde::Deserialize;

use crate::{checksum, sha256_file, Error, Result};

const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_BINARY: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ReleaseVersion {
    pub tag: String,
    pub version: Version,
}

impl ReleaseVersion {
    pub fn parse(value: &str) -> Result<Self> {
        let version = Version::parse(value.strip_prefix('v').unwrap_or(value))?;
        Ok(Self {
            tag: format!("v{version}"),
            version,
        })
    }
}

pub fn is_newer(current: &str, available: &ReleaseVersion) -> Result<bool> {
    Ok(available.version > Version::parse(current.trim_start_matches('v'))?)
}

pub fn platform_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(Error::Invalid(
            "No release build for this platform; build from source".into(),
        )),
    }
}

#[derive(Clone)]
pub struct ReleaseClient {
    client: reqwest::Client,
    api: Url,
    downloads: Url,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

impl ReleaseClient {
    pub fn github(timeout: Duration) -> Result<Self> {
        Self::new(
            "https://api.github.com/repos/soketlabs/loop/releases/",
            "https://github.com/soketlabs/loop/releases/download/",
            timeout,
        )
    }

    /// Custom origins are for mirrors and local release fixtures. HTTP is only
    /// accepted on loopback; production origins must use HTTPS.
    pub fn new(api: &str, downloads: &str, timeout: Duration) -> Result<Self> {
        fn origin(value: &str) -> Result<Url> {
            let url = Url::parse(value).map_err(|e| Error::Invalid(e.to_string()))?;
            let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
            if (!local && url.scheme() != "https") || !url.path().ends_with('/') {
                return Err(Error::Invalid(
                    "Release origin must use HTTPS and end with /".into(),
                ));
            }
            Ok(url)
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("loop-update/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(5))
                .timeout(timeout)
                .https_only(!api.starts_with("http:") && !downloads.starts_with("http:"))
                .build()?,
            api: origin(api)?,
            downloads: origin(downloads)?,
        })
    }

    pub async fn latest(&self) -> Result<ReleaseVersion> {
        let body = self
            .small_file(self.endpoint(&self.api, "latest")?, 1024 * 1024)
            .await?;
        let release: GithubRelease = serde_json::from_slice(&body)?;
        if release.draft || release.prerelease {
            return Err(Error::Invalid(
                "Latest release is not a stable published release".into(),
            ));
        }
        ReleaseVersion::parse(&release.tag_name)
    }

    fn endpoint(&self, origin: &Url, path: &str) -> Result<Url> {
        origin
            .join(path)
            .map_err(|e| Error::Invalid(format!("Invalid release URL: {e}")))
    }

    async fn small_file(&self, url: Url, limit: usize) -> Result<Vec<u8>> {
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > limit {
                return Err(Error::Invalid("Release response is too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Download and verify the archive before reading any member. Extract only
    /// the expected regular executable, never paths, links, or other files.
    pub async fn download(
        &self,
        release: &ReleaseVersion,
        dir: &Path,
    ) -> Result<std::path::PathBuf> {
        let target = platform_target()?;
        let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
        let asset = format!("loop-{target}.{ext}");
        let url = self.endpoint(&self.downloads, &format!("{}/{asset}", release.tag))?;
        let hash_url =
            self.endpoint(&self.downloads, &format!("{}/{asset}.sha256", release.tag))?;
        let hash_text = self.small_file(hash_url, 4096).await?;
        let hash = checksum(
            std::str::from_utf8(&hash_text)
                .map_err(|_| Error::Invalid("Checksum is not UTF-8".into()))?,
            &asset,
        )?;
        let archive = dir.join(&asset);
        let mut file = std::fs::File::create(&archive)?;
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await? {
            size += chunk.len() as u64;
            if size > MAX_ARCHIVE {
                return Err(Error::Invalid("Release archive is too large".into()));
            }
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        drop(file);
        if sha256_file(&archive)? != hash {
            return Err(Error::Invalid(
                "Release archive checksum mismatch; current binary kept".into(),
            ));
        }
        let binary = dir.join(if cfg!(windows) { "loop.exe" } else { "loop" });
        extract_binary(&archive, &binary, target)?;
        Ok(binary)
    }
}

fn extract_binary(archive: &Path, binary: &Path, target: &str) -> Result<()> {
    let name = if cfg!(windows) { "loop.exe" } else { "loop" };
    let expected = format!("loop-{target}/{name}");
    let mut found = false;
    let mut copy = |reader: &mut dyn std::io::Read, size: u64| -> Result<()> {
        if found || size > MAX_BINARY {
            return Err(Error::Invalid(
                "Invalid executable in release archive".into(),
            ));
        }
        found = true;
        let mut out = std::fs::File::create(binary)?;
        let written = std::io::copy(&mut reader.take(MAX_BINARY + 1), &mut out)?;
        if written != size {
            return Err(Error::Invalid("Invalid executable size".into()));
        }
        out.sync_all()?;
        Ok(())
    };
    let file = std::fs::File::open(archive)?;
    if cfg!(windows) {
        let mut zip = zip::ZipArchive::new(file)?;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            if entry.name() == expected && entry.is_file() {
                if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
                    return Err(Error::Invalid(
                        "Release executable must not be a symlink".into(),
                    ));
                }
                let size = entry.size();
                copy(&mut entry, size)?;
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in tar.entries()? {
            let mut entry = entry?;
            if entry.path()?.as_ref() == Path::new(&expected) {
                if !entry.header().entry_type().is_file() {
                    return Err(Error::Invalid(
                        "Release executable must be a regular file".into(),
                    ));
                }
                let size = entry.size();
                copy(&mut entry, size)?;
            }
        }
    }
    if !found {
        return Err(Error::Invalid(
            "Release archive does not contain the expected executable".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_numerically_and_reject_paths() {
        assert!(is_newer("0.3.9", &ReleaseVersion::parse("v0.3.10").unwrap()).unwrap());
        assert!(!is_newer("0.4.0", &ReleaseVersion::parse("v0.3.10").unwrap()).unwrap());
        assert!(!is_newer("0.3.4", &ReleaseVersion::parse("v0.3.4").unwrap()).unwrap());
        assert!(ReleaseVersion::parse("../../main").is_err());
        assert!(ReleaseClient::new(
            "http://example.com/",
            "https://example.com/",
            Duration::from_secs(1)
        )
        .is_err());
    }
}

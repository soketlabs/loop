//! Release lookup and download: version types, the release host client, and
//! checksum-verified archive extraction.

use std::{
    io::{Read, Write},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::Url;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{checksum, sha256_file, Error, Result};

const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_BINARY: u64 = 256 * 1024 * 1024;

/// A release version and the Git tag that names it.
#[derive(Debug, Clone)]
pub struct ReleaseVersion {
    /// The tag, always `v` followed by the version, for example `v0.7.0`.
    pub tag: String,
    /// The parsed SemVer version.
    pub version: Version,
}

impl ReleaseVersion {
    /// Parse a version or a tag; see the [`FromStr`](std::str::FromStr) impl.
    pub fn parse(value: &str) -> Result<Self> {
        value.parse()
    }
}

impl std::str::FromStr for ReleaseVersion {
    type Err = Error;

    /// Parse a version or a tag. A leading `v` is optional. Returns an error
    /// when the rest is not valid SemVer.
    fn from_str(value: &str) -> Result<Self> {
        let version = Version::parse(value.strip_prefix('v').unwrap_or(value))?;
        Ok(Self {
            tag: format!("v{version}"),
            version,
        })
    }
}

/// Whether `available` has a higher SemVer precedence than `current`. `current`
/// may have a leading `v`. Returns an error when it is not valid SemVer.
pub fn is_newer(current: &str, available: &ReleaseVersion) -> Result<bool> {
    Ok(Version::parse(current.trim_start_matches('v'))?
        .cmp_precedence(&available.version)
        .is_lt())
}

/// The target triple of the release archive for this platform: Linux (glibc)
/// and macOS, on x86_64 and aarch64. Returns an error on any other platform.
pub fn platform_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(Error::Invalid(
            "No release build for this platform; build from source".into(),
        )),
    }
}

/// A client of the release host. It looks up the latest release, reads notes,
/// and downloads and verifies the archive. All requests have a timeout, and
/// metadata responses have size limits.
#[derive(Clone)]
pub struct ReleaseClient {
    client: reqwest::Client,
    api: Url,
    downloads: Url,
    latest_redirect: Option<Url>,
    metadata_timeout: Duration,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

/// One release's published notes, as GitHub shows them on its page.
#[derive(Debug, Clone)]
pub struct ReleaseNotes {
    /// The release tag.
    pub tag: String,
    /// Markdown; empty when the release has none.
    pub body: String,
    /// The release page.
    pub url: String,
}

#[derive(Deserialize)]
struct GithubReleaseNotes {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
}

#[derive(Serialize, Deserialize)]
struct CachedRelease {
    checked_at: u64,
    tag: String,
}

impl ReleaseClient {
    /// A client of the Loop releases on GitHub. `timeout` bounds each request
    /// (metadata requests use at most 10 seconds of it).
    pub fn github(timeout: Duration) -> Result<Self> {
        Self::new(
            "https://api.github.com/repos/soketlabs/loop/releases/",
            "https://github.com/soketlabs/loop/releases/download/",
            timeout,
        )?
        .with_latest_redirect("https://github.com/soketlabs/loop/releases/latest")
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
            latest_redirect: None,
            metadata_timeout: timeout.min(Duration::from_secs(10)),
        })
    }

    /// Resolve the latest release from the redirect of `url` instead of the
    /// API. `url` must be HTTPS (or loopback) and end with `/latest`. `latest`
    /// then sends a `HEAD` request and reads the tag from the final URL, which
    /// must stay on the same host and port.
    pub fn with_latest_redirect(mut self, url: &str) -> Result<Self> {
        let url = Url::parse(url).map_err(|e| Error::Invalid(e.to_string()))?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if (!local && url.scheme() != "https") || !url.path().ends_with("/latest") {
            return Err(Error::Invalid(
                "Latest-release URL must use HTTPS and end with /latest".into(),
            ));
        }
        self.latest_redirect = Some(url);
        Ok(self)
    }

    /// The latest stable release. Drafts and prereleases are an error when the
    /// API is used.
    pub async fn latest(&self) -> Result<ReleaseVersion> {
        if let Some(url) = &self.latest_redirect {
            let response = self
                .client
                .head(url.clone())
                .timeout(self.metadata_timeout)
                .send()
                .await?
                .error_for_status()?;
            let final_url = response.url();
            let prefix = format!("{}tag/", url.path().trim_end_matches("latest"));
            if url.host_str() != final_url.host_str()
                || url.port_or_known_default() != final_url.port_or_known_default()
            {
                return Err(Error::Invalid(
                    "Latest release redirected to a different origin".into(),
                ));
            }
            let tag = final_url.path().strip_prefix(&prefix).ok_or_else(|| {
                Error::Invalid("Latest release did not resolve to a release tag".into())
            })?;
            let tag = percent_encoding::percent_decode_str(tag)
                .decode_utf8()
                .map_err(|_| Error::Invalid("Invalid release tag encoding".into()))?;
            return ReleaseVersion::parse(&tag);
        }
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

    /// Startup checks reuse a valid result for one day. Explicit update commands
    /// call latest directly. Cache I/O failure never prevents release lookup.
    pub async fn latest_cached(&self, path: &Path) -> Result<ReleaseVersion> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Ok(file) = std::fs::File::open(path) {
            let mut bytes = Vec::new();
            if file.take(4097).read_to_end(&mut bytes).is_ok() && bytes.len() <= 4096 {
                if let Ok(cache) = serde_json::from_slice::<CachedRelease>(&bytes) {
                    if cache.checked_at <= now && now - cache.checked_at < 24 * 60 * 60 {
                        if let Ok(release) = ReleaseVersion::parse(&cache.tag) {
                            return Ok(release);
                        }
                    }
                }
            }
        }
        let release = self.latest().await?;
        if let Some(parent) = path.parent() {
            let write = || -> Result<()> {
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                serde_json::to_writer(
                    &mut file,
                    &CachedRelease {
                        checked_at: now,
                        tag: release.tag.clone(),
                    },
                )?;
                file.persist(path).map_err(|e| e.error)?;
                Ok(())
            };
            let _ = write();
        }
        Ok(release)
    }

    /// The notes published with `release`.
    pub async fn notes(&self, release: &ReleaseVersion) -> Result<ReleaseNotes> {
        let url = self.endpoint(&self.api, &format!("tags/{}", release.tag))?;
        let body = self.small_file(url, 1024 * 1024).await?;
        let notes: GithubReleaseNotes = serde_json::from_slice(&body)?;
        Ok(ReleaseNotes {
            tag: notes.tag_name,
            body: notes.body.unwrap_or_default(),
            url: notes.html_url,
        })
    }

    fn endpoint(&self, origin: &Url, path: &str) -> Result<Url> {
        origin
            .join(path)
            .map_err(|e| Error::Invalid(format!("Invalid release URL: {e}")))
    }

    async fn small_file(&self, url: Url, limit: usize) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get(url)
            .timeout(self.metadata_timeout)
            .send()
            .await?
            .error_for_status()?;
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
    ///
    /// The archive is `loop-<target>.tar.gz` for [`platform_target`], checked
    /// against its `.sha256` file. Returns the path of the extracted `loop`
    /// binary inside `dir`.
    pub async fn download(
        &self,
        release: &ReleaseVersion,
        dir: &Path,
    ) -> Result<std::path::PathBuf> {
        self.download_with_progress(release, dir, |_, _| {}).await
    }

    /// Like [`ReleaseClient::download`]. `progress` is called after each chunk
    /// with the bytes downloaded so far and the total size, when the server
    /// gives one. The archive is limited to 256 MiB.
    pub async fn download_with_progress(
        &self,
        release: &ReleaseVersion,
        dir: &Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<std::path::PathBuf> {
        let target = platform_target()?;
        let asset = format!("loop-{target}.tar.gz");
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
        let total = response.content_length();
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await? {
            size += chunk.len() as u64;
            if size > MAX_ARCHIVE {
                return Err(Error::Invalid("Release archive is too large".into()));
            }
            file.write_all(&chunk)?;
            progress(size, total);
        }
        file.sync_all()?;
        drop(file);
        if sha256_file(&archive)? != hash {
            return Err(Error::Invalid(
                "Release archive checksum mismatch; current binary kept".into(),
            ));
        }
        let binary = dir.join("loop");
        extract_binary(&archive, &binary, target)?;
        Ok(binary)
    }
}

fn extract_binary(archive: &Path, binary: &Path, target: &str) -> Result<()> {
    let expected = format!("loop-{target}/loop");
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
    {
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
    fn release_version_from_str() {
        let with_tag: ReleaseVersion = "v0.7.0".parse().unwrap();
        let bare: ReleaseVersion = "0.7.0".parse().unwrap();
        assert_eq!(with_tag.tag, "v0.7.0");
        assert_eq!(bare.tag, "v0.7.0");
        assert_eq!(with_tag.version, bare.version);
        assert!("latest".parse::<ReleaseVersion>().is_err());
        assert!("".parse::<ReleaseVersion>().is_err());
    }

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

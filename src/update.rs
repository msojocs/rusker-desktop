//! GitHub release metadata and verified downloads, independent of the UI.
//!
//! Call these async methods on the application's existing Tokio runtime. Keep
//! `DownloadedUpdate` alive for as long as its installer path is needed. This
//! module never executes the downloaded file.

use std::{cmp::Ordering, path::PathBuf, time::Duration};

use directories::ProjectDirs;
use futures_util::StreamExt;
use reqwest::{header, redirect::Policy, Client, Response, StatusCode};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, sync::watch};
use url::Url;

pub const CURRENT_VERSION: &str = env!("RUSKER_APP_VERSION");

const API_ROOT: &str = "https://api.github.com/repos/msojocs/rusker-desktop";
const DOWNLOAD_PREFIX: &str = "/msojocs/rusker-desktop/releases/download/";
const INSTALLER_PREFIX: &str = "rusker-desktop-";
const INSTALLER_SUFFIX: &str = "-windows-x64-setup.exe";
const CHECKSUM_NAME: &str = "SHA256SUMS.txt";
const METADATA_LIMIT: usize = 2 * 1024 * 1024;
const CHECKSUM_LIMIT: usize = 1024 * 1024;
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateChannel {
    Stable,
    Preview,
}

impl UpdateChannel {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Stable => "/releases/latest",
            Self::Preview => "/releases/tags/preview",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReleaseUpdate {
    pub channel: UpdateChannel,
    pub version: String,
    pub release_url: String,
    pub release_notes: String,
    pub asset_name: String,
    pub size: u64,
    // Keep the checked assets private so callers cannot supply arbitrary URLs.
    asset: ReleaseAsset,
    checksum: ReleaseAsset,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug)]
pub struct DownloadedUpdate {
    pub path: PathBuf,
    _directory: TempDir,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Automatic updates support only Windows x86_64")]
    UnsupportedPlatform,
    #[error("Invalid application or release version: {0}")]
    InvalidVersion(String),
    #[error("Invalid GitHub release: {0}")]
    InvalidRelease(String),
    #[error("GitHub API rate limit exceeded; try again later")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("Release asset was not found (HTTP 404)")]
    NotFound,
    #[error("GitHub request failed (HTTP {0})")]
    HttpStatus(u16),
    #[error("Update request failed: {0}")]
    Network(#[source] reqwest::Error),
    #[error("Release response exceeds the allowed size")]
    ResponseTooLarge,
    #[error("Invalid release metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("Unsafe release download URL")]
    UnsafeDownloadUrl,
    #[error("No Windows x64 installer was found for this release")]
    MissingInstaller,
    #[error("Release contains ambiguous Windows x64 installers")]
    AmbiguousInstaller,
    #[error("Release is missing SHA256SUMS.txt")]
    MissingChecksum,
    #[error("Invalid SHA256SUMS.txt: {0}")]
    InvalidChecksum(String),
    #[error("Downloaded installer size does not match release metadata (expected {expected}, received {actual})")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("Downloaded installer SHA-256 checksum does not match")]
    ChecksumMismatch,
    #[error("Application cache directory is unavailable")]
    CacheUnavailable,
    #[error("Cannot save update: {0}")]
    Io(#[from] std::io::Error),
}

fn network(error: reqwest::Error) -> UpdateError {
    // Redirect URLs may contain signed CDN query parameters.
    UpdateError::Network(error.without_url())
}

#[derive(Clone, Debug)]
pub struct UpdateService {
    client: Client,
}

impl UpdateService {
    pub fn new() -> Result<Self, UpdateError> {
        let client = Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .timeout(DOWNLOAD_TIMEOUT)
            .user_agent(concat!("rusker-desktop/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    attempt.error("too many GitHub redirects")
                } else if allowed_redirect(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.error("unsafe GitHub redirect")
                }
            }))
            .build()
            .map_err(network)?;
        Ok(Self { client })
    }

    /// `None` means the channel has no published release or no newer version.
    /// Unsupported platforms fail explicitly before making a network request.
    pub async fn check(
        &self,
        channel: UpdateChannel,
    ) -> Result<Option<ReleaseUpdate>, UpdateError> {
        ensure_supported_platform()?;
        let current = parse_version(CURRENT_VERSION)?;
        let response = self
            .client
            .get(format!("{API_ROOT}{}", channel.endpoint()))
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(METADATA_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let bytes = read_limited(check_status(response)?, METADATA_LIMIT).await?;
        let release: GithubRelease = serde_json::from_slice(&bytes)?;
        select_update(release, channel, &current)
    }

    /// Streams a verified installer into a private application-cache directory.
    /// Failure or cancellation drops the TempDir and removes partial files.
    /// Dropping the progress receiver does not cancel the download.
    pub async fn download(
        &self,
        update: ReleaseUpdate,
        progress: watch::Sender<DownloadProgress>,
    ) -> Result<DownloadedUpdate, UpdateError> {
        ensure_supported_platform()?;
        validate_asset_url(&update.asset)?;
        validate_asset_url(&update.checksum)?;
        let version = parse_version(&update.version)?;
        if update.asset_name != update.asset.name
            || update.size != update.asset.size
            || update.asset.name != installer_name(&version)
            || update.size == 0
        {
            return Err(UpdateError::InvalidRelease(
                "installer metadata changed".into(),
            ));
        }
        progress.send_replace(DownloadProgress {
            downloaded_bytes: 0,
            total_bytes: update.size,
        });

        let response = self
            .client
            .get(&update.checksum.browser_download_url)
            .timeout(METADATA_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        let checksum_bytes = read_limited(check_status(response)?, CHECKSUM_LIMIT).await?;
        if checksum_bytes.len() as u64 != update.checksum.size {
            return Err(UpdateError::InvalidChecksum(
                "manifest size does not match release metadata".into(),
            ));
        }
        let checksum_text = std::str::from_utf8(&checksum_bytes)
            .map_err(|_| UpdateError::InvalidChecksum("manifest is not UTF-8".into()))?;
        let expected_hash = parse_checksum(checksum_text, &update.asset.name)?;

        let directories = ProjectDirs::from("dev", "Rusker", "Rusker Desktop")
            .ok_or(UpdateError::CacheUnavailable)?;
        let cache = directories.cache_dir().join("updates");
        tokio::fs::create_dir_all(&cache).await?;
        let directory = tempfile::Builder::new()
            .prefix("download-")
            .tempdir_in(cache)?;
        let partial_path = directory.path().join("installer.partial");
        let path = directory.path().join(&update.asset.name);
        let response = self
            .client
            .get(&update.asset.browser_download_url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        let response = check_status(response)?;
        if let Some(actual) = response.content_length() {
            if actual != update.size {
                return Err(UpdateError::SizeMismatch {
                    expected: update.size,
                    actual,
                });
            }
        }
        let mut file = tokio::fs::File::create(&partial_path).await?;
        let mut stream = response.bytes_stream();
        let mut hash = Sha256::new();
        let mut downloaded = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(network)?;
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or(UpdateError::ResponseTooLarge)?;
            if downloaded > update.size {
                return Err(UpdateError::SizeMismatch {
                    expected: update.size,
                    actual: downloaded,
                });
            }
            file.write_all(&chunk).await?;
            hash.update(&chunk);
            progress.send_replace(DownloadProgress {
                downloaded_bytes: downloaded,
                total_bytes: update.size,
            });
        }
        if downloaded != update.size {
            return Err(UpdateError::SizeMismatch {
                expected: update.size,
                actual: downloaded,
            });
        }
        let actual_hash: [u8; 32] = hash.finalize().into();
        if actual_hash != expected_hash {
            return Err(UpdateError::ChecksumMismatch);
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(partial_path, &path).await?;
        Ok(DownloadedUpdate {
            path,
            _directory: directory,
        })
    }
}

fn ensure_supported_platform() -> Result<(), UpdateError> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok(())
    } else {
        Err(UpdateError::UnsupportedPlatform)
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

fn parse_version(value: &str) -> Result<Version, UpdateError> {
    Version::parse(value).map_err(|_| UpdateError::InvalidVersion(value.to_owned()))
}

fn installer_name(version: &Version) -> String {
    format!("{INSTALLER_PREFIX}{version}{INSTALLER_SUFFIX}")
}

fn installer_version(name: &str) -> Option<Version> {
    let value = name
        .strip_prefix(INSTALLER_PREFIX)?
        .strip_suffix(INSTALLER_SUFFIX)?;
    let version = Version::parse(value).ok()?;
    // Only the exact canonical versioned installer filename is accepted.
    (installer_name(&version) == name).then_some(version)
}

fn select_update(
    release: GithubRelease,
    channel: UpdateChannel,
    current: &Version,
) -> Result<Option<ReleaseUpdate>, UpdateError> {
    if release.draft {
        return Err(UpdateError::InvalidRelease(
            "draft releases cannot be installed".into(),
        ));
    }
    let (version, asset) = match channel {
        UpdateChannel::Stable => {
            if release.prerelease {
                return Err(UpdateError::InvalidRelease(
                    "stable channel returned a prerelease".into(),
                ));
            }
            let version = parse_version(
                release
                    .tag_name
                    .strip_prefix('v')
                    .unwrap_or(&release.tag_name),
            )?;
            if !version.pre.is_empty() {
                return Err(UpdateError::InvalidRelease(
                    "stable tag contains a prerelease version".into(),
                ));
            }
            if version.cmp_precedence(current) != Ordering::Greater {
                return Ok(None);
            }
            let name = installer_name(&version);
            let mut candidates = release.assets.iter().filter(|asset| asset.name == name);
            let asset = candidates.next().ok_or(UpdateError::MissingInstaller)?;
            if candidates.next().is_some() {
                return Err(UpdateError::AmbiguousInstaller);
            }
            (version, asset)
        }
        UpdateChannel::Preview => {
            if release.tag_name != "preview" {
                return Err(UpdateError::InvalidRelease(
                    "preview channel returned an unexpected tag".into(),
                ));
            }
            let mut candidates = release
                .assets
                .iter()
                .filter_map(|asset| installer_version(&asset.name).map(|version| (version, asset)));
            let (version, asset) = candidates.next().ok_or(UpdateError::MissingInstaller)?;
            if candidates.next().is_some() {
                return Err(UpdateError::AmbiguousInstaller);
            }
            if version.cmp_precedence(current) != Ordering::Greater {
                return Ok(None);
            }
            (version, asset)
        }
    };
    if asset.size == 0 {
        return Err(UpdateError::InvalidRelease("installer is empty".into()));
    }
    validate_asset_url(asset)?;
    let mut manifests = release
        .assets
        .iter()
        .filter(|asset| asset.name == CHECKSUM_NAME);
    let checksum = manifests.next().ok_or(UpdateError::MissingChecksum)?;
    if manifests.next().is_some() || checksum.size == 0 || checksum.size > CHECKSUM_LIMIT as u64 {
        return Err(UpdateError::InvalidChecksum(
            "ambiguous or invalid manifest size".into(),
        ));
    }
    validate_asset_url(checksum)?;
    // Pin both files to this release tag, including the rolling preview tag.
    let installer_url =
        Url::parse(&asset.browser_download_url).map_err(|_| UpdateError::UnsafeDownloadUrl)?;
    let checksum_url =
        Url::parse(&checksum.browser_download_url).map_err(|_| UpdateError::UnsafeDownloadUrl)?;
    let expected_path = |name: &str| {
        let mut url = Url::parse("https://github.com").expect("static GitHub URL");
        url.path_segments_mut()
            .expect("GitHub URL has path segments")
            .extend([
                "msojocs",
                "rusker-desktop",
                "releases",
                "download",
                &release.tag_name,
                name,
            ]);
        url.path().to_owned()
    };
    if installer_url.path() != expected_path(&asset.name)
        || checksum_url.path() != expected_path(&checksum.name)
    {
        return Err(UpdateError::UnsafeDownloadUrl);
    }
    let mut release_url = Url::parse("https://github.com/msojocs/rusker-desktop/releases/tag/")
        .expect("static release URL");
    release_url
        .path_segments_mut()
        .expect("GitHub URL has path segments")
        .pop_if_empty()
        .push(&release.tag_name);
    Ok(Some(ReleaseUpdate {
        channel,
        version: version.to_string(),
        release_url: release_url.into(),
        release_notes: release.body.unwrap_or_default(),
        asset_name: asset.name.clone(),
        size: asset.size,
        asset: asset.clone(),
        checksum: checksum.clone(),
    }))
}

fn validate_asset_url(asset: &ReleaseAsset) -> Result<(), UpdateError> {
    let url =
        Url::parse(&asset.browser_download_url).map_err(|_| UpdateError::UnsafeDownloadUrl)?;
    let segments: Vec<_> = url
        .path_segments()
        .ok_or(UpdateError::UnsafeDownloadUrl)?
        .collect();
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with(DOWNLOAD_PREFIX)
        || segments.len() != 6
        || segments[4].is_empty()
        || segments[5] != asset.name
    {
        return Err(UpdateError::UnsafeDownloadUrl);
    }
    Ok(())
}

fn allowed_redirect(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && url.host_str().is_some_and(|host| {
            host == "github.com"
                || host == "api.github.com"
                || host.ends_with(".githubusercontent.com")
        })
}

fn check_status(response: Response) -> Result<Response, UpdateError> {
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0"))
    {
        let retry_after_seconds = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        return Err(UpdateError::RateLimited {
            retry_after_seconds,
        });
    }
    if status == StatusCode::NOT_FOUND {
        return Err(UpdateError::NotFound);
    }
    if status != StatusCode::OK {
        return Err(UpdateError::HttpStatus(status.as_u16()));
    }
    Ok(response)
}

async fn read_limited(response: Response, limit: usize) -> Result<Vec<u8>, UpdateError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(UpdateError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network)?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(UpdateError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_checksum(text: &str, asset_name: &str) -> Result<[u8; 32], UpdateError> {
    let mut found = None;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let bytes = line.as_bytes();
        if bytes.len() < 67
            || !bytes[..64].iter().all(u8::is_ascii_hexdigit)
            || bytes[64] != b' '
            || !matches!(bytes[65], b' ' | b'*')
        {
            return Err(UpdateError::InvalidChecksum(
                "invalid sha256sum record".into(),
            ));
        }
        // The preceding ASCII check also establishes this UTF-8 boundary.
        if &line[66..] == asset_name {
            if found.is_some() {
                return Err(UpdateError::InvalidChecksum(
                    "duplicate installer checksum".into(),
                ));
            }
            let mut hash = [0; 32];
            for (index, byte) in hash.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&line[index * 2..index * 2 + 2], 16)
                    .map_err(|_| UpdateError::InvalidChecksum("invalid SHA-256 digest".into()))?;
            }
            found = Some(hash);
        }
    }
    found.ok_or_else(|| UpdateError::InvalidChecksum("installer checksum is missing".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(tag: &str, name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            size: 123,
            browser_download_url: format!("https://github.com{DOWNLOAD_PREFIX}{tag}/{name}"),
        }
    }

    fn release(tag: &str, version: &str, prerelease: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            body: Some("Release notes".into()),
            assets: vec![
                asset(tag, &installer_name(&parse_version(version).unwrap())),
                asset(tag, CHECKSUM_NAME),
            ],
        }
    }

    fn select(
        release: GithubRelease,
        channel: UpdateChannel,
        current: &str,
    ) -> Result<Option<ReleaseUpdate>, UpdateError> {
        select_update(release, channel, &parse_version(current).unwrap())
    }

    #[test]
    fn channel_endpoints_are_fixed() {
        assert_eq!(UpdateChannel::Stable.endpoint(), "/releases/latest");
        assert_eq!(UpdateChannel::Preview.endpoint(), "/releases/tags/preview");
    }

    #[test]
    fn stable_compares_semver_and_ignores_build_metadata() {
        assert!(select(
            release("v1.10.0", "1.10.0", false),
            UpdateChannel::Stable,
            "1.9.0"
        )
        .unwrap()
        .is_some());
        for current in ["1.10.0", "2.0.0", "1.10.0+local"] {
            assert!(select(
                release("v1.10.0", "1.10.0", false),
                UpdateChannel::Stable,
                current
            )
            .unwrap()
            .is_none());
        }
        assert!(select(
            release("v1.0.0", "1.0.0", false),
            UpdateChannel::Stable,
            "1.0.0-preview.9"
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn preview_version_comes_from_installer_not_rolling_tag_or_notes() {
        let mut data = release("preview", "1.2.0-preview.10", true);
        data.body = Some("Version 99.0.0".into());
        let update = select(data, UpdateChannel::Preview, "1.2.0-preview.9")
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "1.2.0-preview.10");
        assert_eq!(
            update.release_url,
            "https://github.com/msojocs/rusker-desktop/releases/tag/preview"
        );
        assert!(select(
            release("preview", "1.2.0-preview.10", true),
            UpdateChannel::Preview,
            "1.2.0"
        )
        .unwrap()
        .is_none());
        assert!(select(
            release("preview", "1.2.0-preview.10", true),
            UpdateChannel::Preview,
            "1.2.0-preview.10+local"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn channel_rules_reject_drafts_wrong_tags_and_stable_prereleases() {
        for channel in [UpdateChannel::Stable, UpdateChannel::Preview] {
            let mut data = release(
                if channel == UpdateChannel::Stable {
                    "v2.0.0"
                } else {
                    "preview"
                },
                "2.0.0",
                false,
            );
            data.draft = true;
            assert!(matches!(
                select(data, channel, "1.0.0"),
                Err(UpdateError::InvalidRelease(_))
            ));
        }
        assert!(matches!(
            select(
                release("v2.0.0", "2.0.0", true),
                UpdateChannel::Stable,
                "1.0.0"
            ),
            Err(UpdateError::InvalidRelease(_))
        ));
        assert!(matches!(
            select(
                release("v2.0.0-rc.1", "2.0.0-rc.1", false),
                UpdateChannel::Stable,
                "1.0.0"
            ),
            Err(UpdateError::InvalidRelease(_))
        ));
        assert!(matches!(
            select(
                release("v2.0.0", "2.0.0", true),
                UpdateChannel::Preview,
                "1.0.0"
            ),
            Err(UpdateError::InvalidRelease(_))
        ));
        assert!(parse_version("preview").is_err());
    }

    #[test]
    fn installer_matching_is_exact_and_architecture_specific() {
        assert_eq!(
            installer_version("rusker-desktop-1.2.3-preview.4-windows-x64-setup.exe")
                .unwrap()
                .to_string(),
            "1.2.3-preview.4"
        );
        for name in [
            "rusker-desktop-v1.2.3-windows-x64-setup.exe",
            "rusker-desktop-1.2-windows-x64-setup.exe",
            "rusker-desktop-01.2.3-windows-x64-setup.exe",
            "rusker-desktop-1.2.3-windows-arm64-setup.exe",
            "rusker-desktop-1.2.3-linux-x64-setup.exe",
            "rusker-desktop-1.2.3-windows-x64-setup.exe.sig",
            "../rusker-desktop-1.2.3-windows-x64-setup.exe",
            "rusker-desktop-1.2.3-windows-x64.exe",
        ] {
            assert!(installer_version(name).is_none(), "{name}");
        }
        let mut data = release("v2.0.0", "2.0.1", false);
        assert!(matches!(
            select(data, UpdateChannel::Stable, "1.0.0"),
            Err(UpdateError::MissingInstaller)
        ));
        data = release("preview", "2.0.0-preview.1", true);
        data.assets.push(asset(
            "preview",
            "rusker-desktop-2.0.0-preview.2-windows-x64-setup.exe",
        ));
        assert!(matches!(
            select(data, UpdateChannel::Preview, "1.0.0"),
            Err(UpdateError::AmbiguousInstaller)
        ));
    }

    #[test]
    fn rejects_missing_duplicate_empty_or_unsafe_assets() {
        let mut data = release("v2.0.0", "2.0.0", false);
        data.assets.pop();
        assert!(matches!(
            select(data, UpdateChannel::Stable, "1.0.0"),
            Err(UpdateError::MissingChecksum)
        ));
        let mut data = release("v2.0.0", "2.0.0", false);
        data.assets.push(data.assets[0].clone());
        assert!(matches!(
            select(data, UpdateChannel::Stable, "1.0.0"),
            Err(UpdateError::AmbiguousInstaller)
        ));
        let mut data = release("v2.0.0", "2.0.0", false);
        data.assets[0].size = 0;
        assert!(matches!(
            select(data, UpdateChannel::Stable, "1.0.0"),
            Err(UpdateError::InvalidRelease(_))
        ));
        let mut data = release("v2.0.0", "2.0.0", false);
        data.assets[1].browser_download_url =
            format!("https://github.com{DOWNLOAD_PREFIX}other/{CHECKSUM_NAME}");
        assert!(matches!(
            select(data, UpdateChannel::Stable, "1.0.0"),
            Err(UpdateError::UnsafeDownloadUrl)
        ));
    }

    #[test]
    fn only_original_repository_https_download_urls_are_accepted() {
        let valid = asset("preview", CHECKSUM_NAME);
        assert!(validate_asset_url(&valid).is_ok());
        for url in [
            "http://github.com/msojocs/rusker-desktop/releases/download/preview/SHA256SUMS.txt",
            "https://github.com.evil.test/msojocs/rusker-desktop/releases/download/preview/SHA256SUMS.txt",
            "https://github.com/other/rusker-desktop/releases/download/preview/SHA256SUMS.txt",
            "https://github.com/msojocs/rusker-desktop/releases/download/preview/not-checksums.txt",
            "https://user@github.com/msojocs/rusker-desktop/releases/download/preview/SHA256SUMS.txt",
            "https://github.com:444/msojocs/rusker-desktop/releases/download/preview/SHA256SUMS.txt",
            "https://github.com/msojocs/rusker-desktop/releases/download/preview/SHA256SUMS.txt?redirect=evil",
            "https://github.com/msojocs/rusker-desktop/releases/download/preview/SHA256SUMS.txt#fragment",
            "https://github.com/msojocs/rusker-desktop/releases/download/preview/../SHA256SUMS.txt",
        ] {
            assert!(validate_asset_url(&ReleaseAsset { browser_download_url: url.into(), ..valid.clone() }).is_err(), "{url}");
        }
    }

    #[test]
    fn redirects_allow_github_https_cdn_only() {
        for url in [
            "https://github.com/file",
            "https://release-assets.githubusercontent.com/file?signature=token",
            "https://objects.githubusercontent.com/file",
        ] {
            assert!(allowed_redirect(&Url::parse(url).unwrap()));
        }
        for url in [
            "http://objects.githubusercontent.com/file",
            "https://evil.test/file",
            "https://githubusercontent.com.evil.test/file",
            "https://fakegithubusercontent.com/file",
            "https://github.com:444/file",
            "https://user@github.com/file",
        ] {
            assert!(!allowed_redirect(&Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn checksum_accepts_gnu_text_binary_and_crlf_records() {
        let name = "rusker-desktop-1.2.3-windows-x64-setup.exe";
        let digest = "ab".repeat(32);
        for separator in ["  ", " *"] {
            let text = format!(
                "{}  other.exe\r\n{digest}{separator}{name}\r\n",
                "00".repeat(32)
            );
            assert_eq!(parse_checksum(&text, name).unwrap(), [0xab; 32]);
        }
        assert_eq!(
            parse_checksum(&format!("{}  {name}", "AB".repeat(32)), name).unwrap(),
            [0xab; 32]
        );
    }

    #[test]
    fn checksum_rejects_bad_hashes_duplicates_and_inexact_names() {
        let name = "installer.exe";
        let digest = "ab".repeat(32);
        for text in [
            String::new(),
            format!("{digest}  other.exe"),
            format!("{digest}  ./installer.exe"),
            format!("{digest}  installer.exe.sig"),
            format!("{digest}  {name}\n{digest} *{name}"),
            format!("{}  {name}", "x".repeat(64)),
            format!("{}  {name}", "é".repeat(32)),
            format!("abc  {name}"),
            format!("{digest} {name}"),
        ] {
            assert!(parse_checksum(&text, name).is_err(), "{text}");
        }
    }

    #[test]
    fn unsupported_platform_is_explicit() {
        assert_eq!(
            ensure_supported_platform().is_ok(),
            cfg!(all(target_os = "windows", target_arch = "x86_64"))
        );
    }

    #[test]
    fn downloaded_update_owns_and_cleans_its_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let path = root.join("installer.exe");
        std::fs::write(&path, b"verified installer").unwrap();
        let downloaded = DownloadedUpdate {
            path,
            _directory: directory,
        };
        assert!(downloaded.path.exists());
        drop(downloaded);
        assert!(!root.exists());
    }
}

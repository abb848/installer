//! Image download and extraction functionality
//!
//! This module provides functions for downloading HAOS images and
//! extracting compressed archives.

use crate::error::{Error, Result};
use crate::types::{
    DeviceManifest, FlashProgress, FlashStage, GitHubRelease, HaosImage, HaosRelease, ImageFormat,
    StableVersionInfo,
};
use crate::{Backend, ProgressCallback, ReleaseSource};
use directories::ProjectDirs;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Files owned by one installation attempt, never paths supplied by a caller.
/// Clones keep background extraction alive until its file handles are closed.
#[derive(Clone, Debug)]
pub struct TemporaryImage {
    directory: std::sync::Arc<ImageDirectory>,
    format: ImageFormat,
}

const IMAGE_OWNER_MARKER: &str = "hai-temporary-image-v1";
const UTM_IMPORT_MARKER: &str = ".utm-import";

#[derive(Debug)]
struct ImageDirectory {
    directory: Option<tempfile::TempDir>,
    lock: Option<std::fs::File>,
}

impl ImageDirectory {
    fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
}

impl Drop for ImageDirectory {
    fn drop(&mut self) {
        // No installer ever adopts an existing temporary directory. Once the
        // last owner releases this lock, startup recovery may also remove it
        // unless an external UTM import still needs the source.
        drop(self.lock.take());
        if let Some(directory) = self.directory.take() {
            if let Err(error) = remove_image_directory(&directory.keep()) {
                eprintln!("Could not remove temporary image directory: {error}");
            }
        }
    }
}

impl TemporaryImage {
    /// Create a private, locked directory for one installation's image files.
    pub fn new(cache_dir: &Path, format: ImageFormat) -> Result<Self> {
        use std::io::Write;
        let directory = tempfile::Builder::new()
            .prefix("hai-image-")
            .rand_bytes(16)
            .tempdir_in(cache_dir)?;
        let mut lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join(".owner"))?;
        lock.lock()?;
        lock.write_all(IMAGE_OWNER_MARKER.as_bytes())?;
        Ok(Self {
            directory: std::sync::Arc::new(ImageDirectory {
                directory: Some(directory),
                lock: Some(lock),
            }),
            format,
        })
    }

    /// Path of the extracted image inside this installation's directory.
    pub fn path(&self) -> PathBuf {
        let extension = match self.format {
            ImageFormat::Raw => "img",
            ImageFormat::Qcow2 => "qcow2",
        };
        // The unique name also prevents overwriting another Proxmox import.
        self.directory.path().join(format!(
            "{}.{}",
            self.directory.path().file_name().unwrap().to_string_lossy(),
            extension
        ))
    }

    /// Path used to download the compressed archive before extraction.
    pub fn archive_path(&self) -> PathBuf {
        self.directory.path().join("download.xz")
    }

    /// Persist before dispatch: UTM can outlive the installer or its AppleEvent.
    /// An unconfirmed import requires manual cleanup after UTM has finished.
    pub fn begin_utm_import(&self) -> Result<()> {
        let marker = self.directory.path().join(UTM_IMPORT_MARKER);
        let file = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        if let Err(error) = file.sync_all() {
            drop(file);
            // No command was sent. Undo only the marker we just created.
            if let Err(cleanup_error) = std::fs::remove_file(&marker) {
                eprintln!(
                    "Could not remove UTM import marker {}: {cleanup_error}",
                    marker.display()
                );
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Allow cleanup only after UTM has replied with completion or rejection.
    pub fn finish_utm_import(&self) -> Result<()> {
        std::fs::remove_file(self.directory.path().join(UTM_IMPORT_MARKER))?;
        Ok(())
    }

    /// Publish only an archive which extracted successfully. Cache maintenance
    /// cannot affect in-flight downloads or extraction in private directories.
    pub fn cache_archive(&self, cache_dir: &Path, board: &str, version: &str) {
        if !valid_board(board) || parse_version(version).is_none() {
            return;
        }
        let suffix = match self.format {
            ImageFormat::Raw => "img.xz",
            ImageFormat::Qcow2 => "qcow2.xz",
        };
        let destination = cache_dir.join(format!("haos_{board}-{version}.{suffix}"));
        if let Err(error) = std::fs::rename(self.archive_path(), destination) {
            eprintln!("Could not retain compressed image in cache: {error}");
        }
    }
}

fn valid_board(board: &str) -> bool {
    !board.is_empty()
        && board
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

fn parse_version(version: &str) -> Option<Vec<u64>> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() < 2
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    parts.into_iter().map(|part| part.parse().ok()).collect()
}

/// Keep the newest stable archive for each exact board and image format.
/// Also recover owned temporary directories after a process exit. Unknown
/// names, prereleases, unowned directories, and symlinks are left alone.
pub fn prune_cached_images(cache_dir: &Path) -> Result<()> {
    let mut newest = std::collections::HashMap::<(String, String), (Vec<u64>, PathBuf)>::new();
    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_abandoned_image(&entry.path());
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(|name| name.strip_prefix("haos_")) else {
            continue;
        };
        let Some((stem, suffix)) = [".img.xz", ".qcow2.xz"]
            .iter()
            .find_map(|suffix| name.strip_suffix(suffix).map(|stem| (stem, *suffix)))
        else {
            continue;
        };
        let Some((board, version)) = stem.rsplit_once('-') else {
            continue;
        };
        let Some(version) = parse_version(version) else {
            continue;
        };
        if !valid_board(board) {
            continue;
        }
        let key = (board.to_owned(), suffix.to_owned());
        if let Some((previous_version, previous_path)) = newest.get_mut(&key) {
            let obsolete = if version > *previous_version {
                *previous_version = version;
                std::mem::replace(previous_path, entry.path())
            } else {
                entry.path()
            };
            if let Err(error) = std::fs::remove_file(&obsolete) {
                eprintln!(
                    "Could not remove old cached image {}: {error}",
                    obsolete.display()
                );
            }
        } else {
            newest.insert(key, (version, entry.path()));
        }
    }
    Ok(())
}

fn remove_abandoned_image(path: &Path) {
    let Some(suffix) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("hai-image-"))
    else {
        return;
    };
    if suffix.len() != 16 || !suffix.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return;
    }
    let marker = path.join(".owner");
    if !std::fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.is_file()) {
        return;
    }
    let Ok(mut lock) = std::fs::File::options().read(true).write(true).open(marker) else {
        return;
    };
    if lock.try_lock().is_err() {
        return;
    }
    use std::io::Read;
    let mut contents = String::new();
    if lock
        .by_ref()
        .take(64)
        .read_to_string(&mut contents)
        .is_err()
        || contents != IMAGE_OWNER_MARKER
    {
        return;
    }
    drop(lock);
    if let Err(error) = remove_image_directory(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "Could not remove abandoned image {}: {error}",
                path.display()
            );
        }
    }
}

fn remove_image_directory(path: &Path) -> std::io::Result<()> {
    // An AppleEvent timeout or installer exit does not cancel UTM's import.
    // Retain these sources across both owner drop and startup recovery.
    if path.join(UTM_IMPORT_MARKER).try_exists()? {
        return Ok(());
    }
    // Keep the ownership marker if a busy file prevents cleanup, so startup
    // can retry instead of treating the partially removed directory as unowned.
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == ".owner" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    std::fs::remove_file(path.join(".owner"))?;
    std::fs::remove_dir(path)
}

/// Home Assistant version API for stable releases
const VERSION_URL: &str = "https://version.home-assistant.io/stable.json";

/// GitHub API URL for HAOS releases
const HAOS_RELEASES_API: &str =
    "https://api.github.com/repos/home-assistant/operating-system/releases";

/// User agent for API requests
const USER_AGENT: &str = concat!("HomeAssistantInstaller/", env!("CARGO_PKG_VERSION"));

/// How often to send progress updates (every N bytes)
const PROGRESS_UPDATE_INTERVAL: u64 = 10 * 1024 * 1024; // 10 MB

/// Get the cache directory for downloaded images
pub(crate) fn get_cache_dir() -> Result<PathBuf> {
    let project_dirs = ProjectDirs::from("io", "home-assistant", "installer")
        .ok_or_else(|| Error::InvalidConfig("Could not determine cache directory".to_string()))?;

    let cache_dir = project_dirs.cache_dir().to_path_buf();
    std::fs::create_dir_all(&cache_dir)?;

    Ok(cache_dir)
}

/// Fetch the device manifest
async fn get_device_manifest() -> Result<DeviceManifest> {
    // For now, return the manifest bundled with the installer
    // TODO: Implement actual network fetch
    Ok(crate::manifest::bundled_manifest())
}

/// Check if cache should be skipped via environment variable
pub fn should_skip_cache() -> bool {
    std::env::var("HA_INSTALLER_NO_CACHE")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// Get the path where an image would be cached
pub fn get_cached_image_path(image: &HaosImage) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;
    let filename = image
        .download_url
        .rsplit('/')
        .next()
        .unwrap_or("image.img.xz");

    Ok(cache_dir.join(filename))
}

/// Check if an image is already cached and valid
pub async fn is_cached(image: &HaosImage) -> Result<bool> {
    // Allow skipping cache via environment variable
    if should_skip_cache() {
        return Ok(false);
    }

    let cache_path = get_cached_image_path(image)?;

    if !cache_path.exists() {
        return Ok(false);
    }

    // First check file size (fast)
    let metadata = fs::metadata(&cache_path).await?;
    if metadata.len() != image.size {
        return Ok(false);
    }

    // Size matches. A truncated or corrupt cache entry is caught later when the
    // `.xz` container fails its integrity check during extraction.
    Ok(true)
}

/// Clean up old cached images (partial downloads)
pub async fn cleanup_cache() -> Result<()> {
    let cache_dir = get_cache_dir()?;

    if !cache_dir.exists() {
        return Ok(());
    }

    let mut entries = fs::read_dir(&cache_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        // Remove partial downloads
        if path.extension().is_some_and(|ext| ext == "part") {
            let _ = fs::remove_file(path).await;
        }
    }

    Ok(())
}

/// Fetch the stable version info from Home Assistant (internal version with custom URL)
async fn get_stable_version_from_url(url: &str) -> Result<StableVersionInfo> {
    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(Error::DownloadFailed(format!(
            "Failed to fetch version info: HTTP {}",
            response.status()
        )));
    }

    let version_info: StableVersionInfo = response.json().await?;
    Ok(version_info)
}

/// Fetch the stable version info from Home Assistant
pub(crate) async fn get_stable_version() -> Result<StableVersionInfo> {
    get_stable_version_from_url(VERSION_URL).await
}

/// Get the latest stable HAOS version from the version API
async fn get_latest_haos_version() -> Result<String> {
    let version_info = get_stable_version().await?;

    newest_version(version_info.hassos.values())
        .ok_or_else(|| Error::DownloadFailed("No HAOS versions found in stable.json".to_string()))
}

/// The HAOS version stable.json lists for `board`.
///
/// Boards can be held back during a staged rollout, so the version a board
/// should get is its own entry, not whatever another board is on.
async fn get_latest_haos_version_for_board(board: &str) -> Result<String> {
    version_for_board(&get_stable_version().await?, board)
}

fn version_for_board(version_info: &StableVersionInfo, board: &str) -> Result<String> {
    version_info.hassos.get(board).cloned().ok_or_else(|| {
        Error::DownloadFailed(format!(
            "Home Assistant OS has no current release for board: {}",
            board
        ))
    })
}

/// The newest of a set of HAOS versions (`18.3` beats `9.5`), so the answer
/// doesn't depend on HashMap order.
fn newest_version<'a>(versions: impl Iterator<Item = &'a String>) -> Option<String> {
    fn numeric(version: &str) -> Vec<u32> {
        version
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    }

    versions.max_by_key(|version| numeric(version)).cloned()
}

/// Fetch the latest HAOS release information
async fn fetch_latest_release() -> Result<HaosRelease> {
    let version = get_latest_haos_version().await?;
    fetch_release(&version).await
}

/// Fetch a specific HAOS release by version (internal version with custom base URL)
async fn fetch_release_from_api(api_base_url: &str, version: &str) -> Result<HaosRelease> {
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/tags/{}", api_base_url, version))
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(Error::DownloadFailed(format!(
            "Failed to fetch release {}: HTTP {}",
            version,
            response.status()
        )));
    }

    let release: GitHubRelease = response.json().await?;
    parse_github_release(release)
}

/// Fetch a specific HAOS release by version
async fn fetch_release(version: &str) -> Result<HaosRelease> {
    fetch_release_from_api(HAOS_RELEASES_API, version).await
}

/// Fetch HAOS release info for a specific version (or "latest")
async fn get_haos_release(version: &str) -> Result<HaosRelease> {
    if version == "latest" {
        fetch_latest_release().await
    } else {
        fetch_release(version).await
    }
}

/// Parse a GitHub release into our HaosRelease format
fn parse_github_release(release: GitHubRelease) -> Result<HaosRelease> {
    let version = release.tag_name;
    let mut images = Vec::new();

    for asset in release.assets {
        // Process .img.xz and .qcow2.xz files
        let (suffix, format) = if asset.name.ends_with(".img.xz") {
            (".img.xz", ImageFormat::Raw)
        } else if asset.name.ends_with(".qcow2.xz") {
            (".qcow2.xz", ImageFormat::Qcow2)
        } else {
            continue;
        };

        // Parse board name from filename: haos_{board}-{version}.img.xz
        let board = match parse_board_from_filename_with_suffix(&asset.name, &version, suffix) {
            Ok(b) => b,
            Err(_) => continue,
        };

        images.push(HaosImage {
            board,
            format,
            download_url: asset.browser_download_url,
            size: asset.size,
        });
    }

    Ok(HaosRelease { version, images })
}

/// Parse board name from HAOS image filename with a specific suffix
fn parse_board_from_filename_with_suffix(
    filename: &str,
    version: &str,
    file_suffix: &str,
) -> Result<String> {
    // Format: haos_{board}-{version}{file_suffix}
    let prefix = "haos_";
    let suffix = format!("-{}{}", version, file_suffix);

    if !filename.starts_with(prefix) || !filename.ends_with(&suffix) {
        return Err(Error::InvalidConfig(format!(
            "Invalid filename format: {}",
            filename
        )));
    }

    let board = filename
        .strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(&suffix))
        .ok_or_else(|| Error::InvalidConfig(format!("Cannot parse board from: {}", filename)))?;

    Ok(board.to_string())
}

/// Parse board name from HAOS image filename (convenience wrapper for .img.xz)
pub fn parse_board_from_filename(filename: &str, version: &str) -> Result<String> {
    parse_board_from_filename_with_suffix(filename, version, ".img.xz")
}

/// Download an image file with progress updates
pub(crate) async fn download_image<P: ProgressCallback>(
    url: &str,
    dest_path: &Path,
    progress_callback: &P,
) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        return Err(Error::DownloadFailed(format!(
            "HTTP {} for {}",
            response.status(),
            url
        )));
    }

    let total_size = response.content_length().unwrap_or(0);

    progress_callback.on_progress(FlashProgress {
        stage: FlashStage::Downloading,
        progress: 0,
        bytes_processed: 0,
        total_bytes: total_size,
        message: "Starting download...".to_string(),
    });

    let mut file = std::fs::File::create(dest_path)?;
    let mut downloaded: u64 = 0;
    let mut last_progress_update: u64 = 0;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;

        use std::io::Write;
        file.write_all(&chunk)?;

        downloaded += chunk.len() as u64;

        // Send progress update every PROGRESS_UPDATE_INTERVAL bytes
        if downloaded - last_progress_update >= PROGRESS_UPDATE_INTERVAL {
            let progress = if total_size > 0 {
                ((downloaded as f64 / total_size as f64) * 100.0) as u8
            } else {
                0
            };

            progress_callback.on_progress(FlashProgress {
                stage: FlashStage::Downloading,
                progress,
                bytes_processed: downloaded,
                total_bytes: total_size,
                message: "Downloading image...".to_string(),
            });
            last_progress_update = downloaded;
        }
    }

    progress_callback.on_progress(FlashProgress {
        stage: FlashStage::Downloading,
        progress: 100,
        bytes_processed: downloaded,
        total_bytes: total_size,
        message: "Download complete".to_string(),
    });

    Ok(())
}

/// Extract a .xz compressed file
pub(crate) async fn extract_xz<P: ProgressCallback>(
    archive_path: &Path,
    dest_path: &Path,
    progress_callback: &P,
) -> Result<()> {
    extract_xz_owned(archive_path, dest_path, progress_callback, None).await
}

async fn extract_xz_owned<P: ProgressCallback>(
    archive_path: &Path,
    dest_path: &Path,
    progress_callback: &P,
    owner: Option<TemporaryImage>,
) -> Result<()> {
    use std::sync::mpsc;

    // For extraction, we don't know the final size upfront (xz doesn't store it)
    // Use 0 for total_bytes to signal indeterminate progress
    progress_callback.on_progress(FlashProgress {
        stage: FlashStage::Extracting,
        progress: 0,
        bytes_processed: 0,
        total_bytes: 0,
        message: "Extracting image...".to_string(),
    });

    // Create channel for progress updates
    let (progress_tx, progress_rx) = mpsc::channel::<u64>();

    let archive_path_clone = archive_path.to_path_buf();
    let dest_path_clone = dest_path.to_path_buf();

    let extract_handle = tokio::task::spawn_blocking(move || {
        let _owner = owner;
        use std::io::{Read, Write};

        let input = std::fs::File::open(&archive_path_clone)?;
        let mut decoder = xz2::read::XzDecoder::new(input);
        let mut output = std::fs::File::create(&dest_path_clone)?;

        let mut buffer = vec![0u8; 64 * 1024]; // 64KB buffer
        let mut bytes_extracted: u64 = 0;
        let mut last_progress_update: u64 = 0;

        loop {
            // A read error here means the `.xz` stream failed its built-in
            // integrity check: the download is corrupt or truncated.
            let bytes_read = match decoder.read(&mut buffer) {
                Ok(n) => n,
                Err(e) => {
                    let _ = std::fs::remove_file(&dest_path_clone);
                    let _ = std::fs::remove_file(&archive_path_clone);
                    return Err(Error::ExtractionFailed(format!(
                        "downloaded image is corrupt or incomplete ({e}); \
                         it has been discarded, please try flashing again"
                    )));
                }
            };
            if bytes_read == 0 {
                break;
            }
            output.write_all(&buffer[..bytes_read])?;
            bytes_extracted += bytes_read as u64;

            // Send progress update every PROGRESS_UPDATE_INTERVAL bytes
            if bytes_extracted - last_progress_update >= PROGRESS_UPDATE_INTERVAL {
                let _ = progress_tx.send(bytes_extracted);
                last_progress_update = bytes_extracted;
            }
        }

        output.sync_all()?;

        Ok::<u64, Error>(bytes_extracted)
    });

    // Forward progress updates while waiting for extraction to complete
    loop {
        match progress_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(bytes_extracted) => {
                // Use 0 for total_bytes to signal indeterminate progress
                progress_callback.on_progress(FlashProgress {
                    stage: FlashStage::Extracting,
                    progress: 0, // Indeterminate
                    bytes_processed: bytes_extracted,
                    total_bytes: 0,
                    message: "Extracting image...".to_string(),
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if extract_handle.is_finished() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }

    let final_size = extract_handle
        .await
        .map_err(|e| Error::ExtractionFailed(e.to_string()))??;

    progress_callback.on_progress(FlashProgress {
        stage: FlashStage::Extracting,
        progress: 100,
        bytes_processed: final_size,
        total_bytes: final_size,
        message: "Extraction complete".to_string(),
    });

    Ok(())
}

impl ReleaseSource for Backend {
    async fn get_device_manifest(&self) -> Result<DeviceManifest> {
        get_device_manifest().await
    }

    async fn get_haos_release(&self, version: &str) -> Result<HaosRelease> {
        get_haos_release(version).await
    }

    async fn get_latest_haos_release_for_board(&self, board: &str) -> Result<HaosRelease> {
        let version = get_latest_haos_version_for_board(board).await?;
        fetch_release(&version).await
    }

    async fn download_image<P: ProgressCallback>(
        &self,
        url: &str,
        dest_path: &Path,
        progress_callback: &P,
    ) -> Result<()> {
        download_image(url, dest_path, progress_callback).await
    }

    async fn extract_xz<P: ProgressCallback>(
        &self,
        archive_path: &Path,
        dest_path: &Path,
        progress_callback: &P,
    ) -> Result<()> {
        extract_xz(archive_path, dest_path, progress_callback).await
    }

    async fn extract_temporary_image<P: ProgressCallback>(
        &self,
        image: &TemporaryImage,
        progress_callback: &P,
    ) -> Result<()> {
        extract_xz_owned(
            &image.archive_path(),
            &image.path(),
            progress_callback,
            Some(image.clone()),
        )
        .await
    }

    fn cache_dir(&self) -> Result<PathBuf> {
        get_cache_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stable_with(boards: &[(&str, &str)]) -> StableVersionInfo {
        serde_json::from_value(serde_json::json!({
            "hassos": boards
                .iter()
                .map(|(board, version)| (board.to_string(), version.to_string()))
                .collect::<std::collections::HashMap<_, _>>(),
        }))
        .expect("a stable.json with only hassos parses")
    }

    #[test]
    fn test_version_for_board_uses_that_boards_entry() {
        // A board held back during a staged rollout keeps its own version
        let stable = stable_with(&[("rpi5-64", "18.3"), ("odroid-n2", "18.2")]);

        assert_eq!(version_for_board(&stable, "odroid-n2").unwrap(), "18.2");
        assert_eq!(version_for_board(&stable, "rpi5-64").unwrap(), "18.3");
    }

    #[test]
    fn test_version_for_board_rejects_a_board_without_a_release() {
        let stable = stable_with(&[("rpi5-64", "18.3")]);

        match version_for_board(&stable, "tinker") {
            Err(Error::DownloadFailed(msg)) => assert!(msg.contains("tinker"), "{msg}"),
            other => panic!("Expected DownloadFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_newest_version_compares_numerically() {
        let versions = ["9.5", "18.3", "18.10", "17.0"].map(String::from);
        assert_eq!(newest_version(versions.iter()).as_deref(), Some("18.10"));
        assert_eq!(newest_version([].iter()), None);
    }
    use crate::types::GitHubAsset;
    use serial_test::serial;

    #[test]
    fn temporary_images_are_isolated_and_live_until_last_owner() {
        let cache = tempfile::tempdir().unwrap();
        let first = TemporaryImage::new(cache.path(), ImageFormat::Qcow2).unwrap();
        let second = TemporaryImage::new(cache.path(), ImageFormat::Qcow2).unwrap();
        let path = first.path();
        std::fs::write(&path, b"first").unwrap();
        std::fs::write(second.path(), b"second").unwrap();
        let worker = first.clone();
        drop(first);
        assert!(path.exists());
        drop(worker);
        assert!(!path.exists());
        assert_eq!(std::fs::read(second.path()).unwrap(), b"second");
        let second_path = second.path();
        drop(second);
        assert!(!second_path.exists());
    }

    #[tokio::test]
    async fn failed_extraction_removes_owned_files() {
        let cache = tempfile::tempdir().unwrap();
        let image = TemporaryImage::new(cache.path(), ImageFormat::Raw).unwrap();
        let path = image.path();
        std::fs::write(image.archive_path(), b"not xz").unwrap();
        assert!(Backend
            .extract_temporary_image(&image, &crate::NoOpProgress)
            .await
            .is_err());
        drop(image);
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn cancelled_attempt_removes_owned_files() {
        let cache = tempfile::tempdir().unwrap();
        let image = TemporaryImage::new(cache.path(), ImageFormat::Raw).unwrap();
        let path = image.path();
        std::fs::write(&path, b"partial").unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _image = image;
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        assert!(!path.exists());
    }

    #[test]
    fn cache_pruning_compares_numeric_versions_per_board_and_format() {
        let cache = tempfile::tempdir().unwrap();
        let retained = [
            "haos_rpi5-64-17.10.img.xz",
            "haos_rpi4-64-16.3.img.xz",
            "haos_generic-x86-64-17.1.img.xz",
            "haos_generic-x86-64-16.0.qcow2.xz",
            "haos_rpi5-64-18.0.rc1.img.xz",
            "haos_rpi5-64.img.xz",
            "user-image.qcow2",
            "unfinished.part",
            "haos_ova-17.0.qcow2",
        ];
        for name in retained
            .iter()
            .chain(["haos_rpi5-64-17.9.img.xz", "haos_rpi5-64-9.12.img.xz"].iter())
        {
            std::fs::write(cache.path().join(name), b"archive").unwrap();
        }
        let active = TemporaryImage::new(cache.path(), ImageFormat::Raw).unwrap();
        std::fs::write(active.archive_path(), b"active").unwrap();
        prune_cached_images(cache.path()).unwrap();
        for name in retained {
            assert!(cache.path().join(name).exists(), "{name}");
        }
        assert!(!cache.path().join("haos_rpi5-64-17.9.img.xz").exists());
        assert!(!cache.path().join("haos_rpi5-64-9.12.img.xz").exists());
        assert!(active.archive_path().exists());
    }

    #[test]
    fn publishing_cache_rejects_unsafe_board_and_version() {
        let cache = tempfile::tempdir().unwrap();
        let image = TemporaryImage::new(cache.path(), ImageFormat::Raw).unwrap();
        std::fs::write(image.archive_path(), b"archive").unwrap();
        image.cache_archive(cache.path(), "../user", "17.0");
        image.cache_archive(cache.path(), "rpi5-64", "../../user");
        assert!(image.archive_path().exists());
        image.cache_archive(cache.path(), "rpi5-64", "17.0");
        assert_eq!(
            std::fs::read(cache.path().join("haos_rpi5-64-17.0.img.xz")).unwrap(),
            b"archive"
        );
        drop(image);
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 1);
    }

    #[test]
    fn startup_reclaims_only_unlocked_owned_directories() {
        use std::io::Write;
        let cache = tempfile::tempdir().unwrap();
        let live = TemporaryImage::new(cache.path(), ImageFormat::Raw).unwrap();
        std::fs::write(live.path(), b"live").unwrap();
        let abandoned = cache.path().join("hai-image-1234567890123456");
        std::fs::create_dir(&abandoned).unwrap();
        std::fs::write(abandoned.join("image.img"), b"abandoned").unwrap();
        let mut lock = std::fs::File::create(abandoned.join(".owner")).unwrap();
        lock.lock().unwrap();
        lock.write_all(IMAGE_OWNER_MARKER.as_bytes()).unwrap();
        let unowned = cache.path().join("hai-image-0000000000000000");
        std::fs::create_dir(&unowned).unwrap();
        std::fs::write(unowned.join(".owner"), b"not an installer image").unwrap();
        prune_cached_images(cache.path()).unwrap();
        assert!(
            abandoned.exists(),
            "separately held lock must protect the directory"
        );
        assert!(live.path().exists());
        // Make the unlocked fixture independent of when all descriptors close.
        lock.unlock().unwrap();
        drop(lock);
        prune_cached_images(cache.path()).unwrap();
        assert!(!abandoned.exists());
        assert!(live.path().exists());
        assert!(unowned.exists());
    }

    #[test]
    #[cfg(unix)]
    fn startup_does_not_follow_directory_or_marker_symlinks() {
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join(".owner"), IMAGE_OWNER_MARKER).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            cache.path().join("hai-image-1234567890123456"),
        )
        .unwrap();
        let linked_marker = cache.path().join("hai-image-0000000000000000");
        std::fs::create_dir(&linked_marker).unwrap();
        std::os::unix::fs::symlink(outside.path().join(".owner"), linked_marker.join(".owner"))
            .unwrap();
        prune_cached_images(cache.path()).unwrap();
        assert!(outside.path().join(".owner").exists());
        assert!(linked_marker.exists());
        assert!(cache.path().join("hai-image-1234567890123456").is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn failed_cleanup_retains_marker_for_startup_retry() {
        use std::os::unix::fs::PermissionsExt;
        let cache = tempfile::tempdir().unwrap();
        let image = TemporaryImage::new(cache.path(), ImageFormat::Raw).unwrap();
        // Hold the owner lock explicitly so each retry sees a known lock state.
        let lock = image.directory.lock.as_ref().unwrap().try_clone().unwrap();
        let directory = image.path().parent().unwrap().to_path_buf();
        let blocked = directory.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("image"), b"data").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o0)).unwrap();
        drop(image);
        // Observe the failed removal itself: opening ReadDir can succeed even
        // when reading or removing entries is denied. Root may remove it all.
        if directory.exists() {
            assert!(directory.join(".owner").exists());
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
            prune_cached_images(cache.path()).unwrap();
            assert!(directory.exists(), "the shared lock must prevent recovery");
            assert_eq!(std::fs::read(blocked.join("image")).unwrap(), b"data");
            lock.unlock().unwrap();
            prune_cached_images(cache.path()).unwrap();
        }
        assert!(!directory.exists());
    }

    #[test]
    fn test_get_cache_dir() {
        let result = get_cache_dir();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_device_manifest_returns_bundled() {
        let manifest = get_device_manifest().await.unwrap();
        let bundled = crate::manifest::bundled_manifest();
        assert_eq!(manifest.version, bundled.version);
        assert_eq!(manifest.devices.len(), bundled.devices.len());
    }

    #[test]
    #[serial]
    fn test_should_skip_cache_true() {
        std::env::set_var("HA_INSTALLER_NO_CACHE", "1");
        assert!(should_skip_cache());
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
    }

    #[test]
    #[serial]
    fn test_should_skip_cache_false() {
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
        assert!(!should_skip_cache());
    }

    #[test]
    fn test_parse_board_from_filename_standard() {
        let result = parse_board_from_filename("haos_rpi5-64-14.2.img.xz", "14.2");
        assert_eq!(result.unwrap(), "rpi5-64");

        let result = parse_board_from_filename("haos_generic-x86-64-14.2.img.xz", "14.2");
        assert_eq!(result.unwrap(), "generic-x86-64");

        let result = parse_board_from_filename("haos_green-14.2.img.xz", "14.2");
        assert_eq!(result.unwrap(), "green");
    }

    #[test]
    fn test_parse_board_from_filename_qcow2() {
        let result = parse_board_from_filename_with_suffix(
            "haos_generic-x86-64-14.2.qcow2.xz",
            "14.2",
            ".qcow2.xz",
        );
        assert_eq!(result.unwrap(), "generic-x86-64");

        let result = parse_board_from_filename_with_suffix(
            "haos_generic-aarch64-14.2.qcow2.xz",
            "14.2",
            ".qcow2.xz",
        );
        assert_eq!(result.unwrap(), "generic-aarch64");
    }

    #[test]
    fn test_parse_board_from_filename_invalid() {
        // Wrong prefix
        let result = parse_board_from_filename("wrong_rpi5-64-14.2.img.xz", "14.2");
        assert!(result.is_err());

        // Wrong suffix
        let result = parse_board_from_filename("haos_rpi5-64-14.2.zip", "14.2");
        assert!(result.is_err());

        // Wrong version
        let result = parse_board_from_filename("haos_rpi5-64-14.2.img.xz", "14.3");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_github_release() {
        let release = GitHubRelease {
            tag_name: "14.2".to_string(),
            assets: vec![
                GitHubAsset {
                    name: "haos_rpi5-64-14.2.img.xz".to_string(),
                    size: 500_000_000,
                    browser_download_url: "https://github.com/download/rpi5.img.xz".to_string(),
                },
                GitHubAsset {
                    name: "haos_generic-x86-64-14.2.qcow2.xz".to_string(),
                    size: 600_000_000,
                    browser_download_url: "https://github.com/download/x86.qcow2.xz".to_string(),
                },
                // Should be ignored (wrong extension)
                GitHubAsset {
                    name: "haos_rpi5-64-14.2.img.xz.sha256".to_string(),
                    size: 100,
                    browser_download_url: "https://github.com/download/sha256".to_string(),
                },
            ],
        };

        let parsed = parse_github_release(release).unwrap();
        assert_eq!(parsed.version, "14.2");
        assert_eq!(parsed.images.len(), 2);

        // Check rpi5-64 image
        let rpi_image = parsed.images.iter().find(|i| i.board == "rpi5-64").unwrap();
        assert_eq!(rpi_image.format, ImageFormat::Raw);
        assert_eq!(rpi_image.size, 500_000_000);

        // Check x86 qcow2 image
        let x86_image = parsed
            .images
            .iter()
            .find(|i| i.board == "generic-x86-64")
            .unwrap();
        assert_eq!(x86_image.format, ImageFormat::Qcow2);
        assert_eq!(x86_image.size, 600_000_000);
    }

    #[tokio::test]
    #[serial]
    async fn test_is_cached_skip_cache_env() {
        std::env::set_var("HA_INSTALLER_NO_CACHE", "1");
        let image = HaosImage {
            board: "test".to_string(),
            format: ImageFormat::Raw,
            download_url: "https://example.com/test.img.xz".to_string(),
            size: 100,
        };
        let result = is_cached(&image).await.unwrap();
        assert!(!result);
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
    }

    #[tokio::test]
    #[serial]
    async fn test_is_cached_file_not_exist() {
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
        let image = HaosImage {
            board: "test".to_string(),
            format: ImageFormat::Raw,
            download_url: "https://example.com/nonexistent-file-12345.img.xz".to_string(),
            size: 100,
        };
        let result = is_cached(&image).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_cleanup_cache_removes_part_files() {
        let cache_dir = get_cache_dir().unwrap();

        // Create a test .part file
        let part_file = cache_dir.join("test_cleanup.img.xz.part");
        std::fs::write(&part_file, b"test").unwrap();
        assert!(part_file.exists());

        // Run cleanup
        cleanup_cache().await.unwrap();

        // Part file should be removed
        assert!(!part_file.exists());
    }

    #[tokio::test]
    async fn test_get_cached_image_path() {
        let image = HaosImage {
            board: "test".to_string(),
            format: ImageFormat::Raw,
            download_url: "https://github.com/home-assistant/operating-system/releases/download/14.2/haos_rpi5-64-14.2.img.xz".to_string(),
            size: 100,
        };

        let path = get_cached_image_path(&image).unwrap();
        assert!(path.to_string_lossy().contains("haos_rpi5-64-14.2.img.xz"));
    }

    #[test]
    fn test_parse_board_from_filename_empty() {
        let result = parse_board_from_filename("", "14.2");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_board_from_filename_no_haos_prefix() {
        let result = parse_board_from_filename("rpi5-64-14.2.img.xz", "14.2");
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_should_skip_cache_true_lowercase() {
        std::env::set_var("HA_INSTALLER_NO_CACHE", "true");
        assert!(should_skip_cache());
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
    }

    #[test]
    #[serial]
    fn test_should_skip_cache_false_with_false_value() {
        std::env::set_var("HA_INSTALLER_NO_CACHE", "false");
        assert!(!should_skip_cache());
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
    }

    #[test]
    #[serial]
    fn test_should_skip_cache_false_with_zero() {
        std::env::set_var("HA_INSTALLER_NO_CACHE", "0");
        assert!(!should_skip_cache());
        std::env::remove_var("HA_INSTALLER_NO_CACHE");
    }

    #[test]
    fn test_get_cached_image_path_url_without_slash() {
        // Edge case: URL without "/" should use fallback filename
        let image = HaosImage {
            board: "test".to_string(),
            format: ImageFormat::Raw,
            download_url: "no-slashes-here".to_string(),
            size: 100,
        };

        let path = get_cached_image_path(&image).unwrap();
        assert!(path.to_string_lossy().contains("no-slashes-here"));
    }

    #[tokio::test]
    #[serial]
    async fn test_is_cached_size_mismatch() {
        std::env::remove_var("HA_INSTALLER_NO_CACHE");

        // Create a temp file with wrong size
        let cache_dir = get_cache_dir().unwrap();
        let test_file = cache_dir.join("test_size_mismatch.img.xz");

        // Write 50 bytes
        std::fs::write(&test_file, [0u8; 50]).unwrap();

        // Image expects 100 bytes
        let image = HaosImage {
            board: "test".to_string(),
            format: ImageFormat::Raw,
            download_url: format!(
                "https://example.com/{}",
                test_file.file_name().unwrap().to_string_lossy()
            ),
            size: 100,
        };

        let result = is_cached(&image).await.unwrap();
        assert!(!result, "Should return false when file size doesn't match");

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    #[serial]
    async fn test_download_image_http_404_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("GET", "/test.img.xz")
            .with_status(404)
            .create_async()
            .await;

        let url = format!("{}/test.img.xz", server.url());
        let cache_dir = get_cache_dir().unwrap();
        let dest = cache_dir.join("test_404.img");

        let result = download_image(&url, &dest, &crate::NoOpProgress).await;
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(matches!(e, crate::error::Error::DownloadFailed(_)));
        }

        mock.assert_async().await;
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    #[serial]
    async fn test_download_image_http_500_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("GET", "/test.img.xz")
            .with_status(500)
            .create_async()
            .await;

        let url = format!("{}/test.img.xz", server.url());
        let cache_dir = get_cache_dir().unwrap();
        let dest = cache_dir.join("test_500.img");

        // Clean up any existing file from previous test runs
        let _ = std::fs::remove_file(&dest);

        let result = download_image(&url, &dest, &crate::NoOpProgress).await;
        assert!(result.is_err());

        mock.assert_async().await;
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    #[serial]
    async fn test_download_image_success_without_checksum() {
        let mut server = mockito::Server::new_async().await;

        let test_data = b"test image data content";
        let mock = server
            .mock("GET", "/test.img.xz")
            .with_status(200)
            .with_header("content-length", &test_data.len().to_string())
            .with_body(test_data.as_slice())
            .create_async()
            .await;

        let url = format!("{}/test.img.xz", server.url());
        let cache_dir = get_cache_dir().unwrap();
        let dest = cache_dir.join("test_download_success.img");

        let result = download_image(&url, &dest, &crate::NoOpProgress).await;
        assert!(result.is_ok());

        // Verify file was created and has correct content
        let content = std::fs::read(&dest).unwrap();
        assert_eq!(content, test_data);

        mock.assert_async().await;
        std::fs::remove_file(&dest).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_download_image_with_progress_updates() {
        use std::sync::{Arc, Mutex};

        struct TestProgressCallback {
            calls: Arc<Mutex<Vec<FlashProgress>>>,
        }

        impl crate::ProgressCallback for TestProgressCallback {
            fn on_progress(&self, progress: FlashProgress) {
                self.calls.lock().unwrap().push(progress);
            }
        }

        let mut server = mockito::Server::new_async().await;

        // Create data larger than PROGRESS_UPDATE_INTERVAL (10MB)
        let test_data = vec![0u8; 11 * 1024 * 1024]; // 11MB

        let mock = server
            .mock("GET", "/large.img.xz")
            .with_status(200)
            .with_header("content-length", &test_data.len().to_string())
            .with_body(&test_data)
            .create_async()
            .await;

        let url = format!("{}/large.img.xz", server.url());
        let cache_dir = get_cache_dir().unwrap();
        let dest = cache_dir.join("test_progress.img");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let callback = TestProgressCallback {
            calls: calls.clone(),
        };

        let result = download_image(&url, &dest, &callback).await;
        assert!(result.is_ok());
        mock.assert_async().await;

        // Check that we got progress callbacks
        let progress_calls = calls.lock().unwrap();
        assert!(!progress_calls.is_empty());
        assert!(progress_calls.iter().any(|p| p.progress == 0)); // Start
        assert!(progress_calls.iter().any(|p| p.progress == 100)); // End
        assert!(progress_calls
            .iter()
            .all(|p| p.stage == FlashStage::Downloading));

        std::fs::remove_file(&dest).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_download_image_no_content_length() {
        let mut server = mockito::Server::new_async().await;

        let test_data = b"small data";
        let mock = server
            .mock("GET", "/test.img.xz")
            .with_status(200)
            // No content-length header
            .with_body(test_data.as_slice())
            .create_async()
            .await;

        let url = format!("{}/test.img.xz", server.url());
        let cache_dir = get_cache_dir().unwrap();
        let dest = cache_dir.join("test_no_length.img");

        let result = download_image(&url, &dest, &crate::NoOpProgress).await;
        assert!(result.is_ok());

        mock.assert_async().await;
        std::fs::remove_file(&dest).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_extract_xz_real_file() {
        use std::io::Write;

        let cache_dir = get_cache_dir().unwrap();
        let test_content = b"Hello, this is test content for XZ compression!";
        let extracted_path = cache_dir.join("test_extracted.txt");
        let archive_path = cache_dir.join("test_archive.txt.xz");

        // Create a real XZ compressed file
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut encoder = xz2::write::XzEncoder::new(file, 6);
            encoder.write_all(test_content).unwrap();
            encoder.finish().unwrap();
        }

        // Extract it
        let result = extract_xz(&archive_path, &extracted_path, &crate::NoOpProgress).await;
        assert!(result.is_ok());

        // Verify extracted content
        let extracted = std::fs::read(&extracted_path).unwrap();
        assert_eq!(extracted, test_content);

        // Cleanup
        std::fs::remove_file(&archive_path).unwrap();
        std::fs::remove_file(&extracted_path).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_extract_xz_nonexistent_file() {
        let cache_dir = get_cache_dir().unwrap();
        let archive_path = cache_dir.join("nonexistent_archive.xz");
        let dest_path = cache_dir.join("output.img");

        let result = extract_xz(&archive_path, &dest_path, &crate::NoOpProgress).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_extract_xz_corrupt_archive_is_discarded() {
        use std::io::Write;

        let cache_dir = get_cache_dir().unwrap();
        let archive_path = cache_dir.join("test_corrupt.img.xz");
        let extracted_path = cache_dir.join("test_corrupt_extracted.img");

        // Write a valid xz stream, then truncate its tail so the container's
        // integrity check fails part-way through decoding.
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut encoder = xz2::write::XzEncoder::new(file, 1);
            encoder.write_all(&vec![0u8; 256 * 1024]).unwrap();
            encoder.finish().unwrap();
        }
        let mut bytes = std::fs::read(&archive_path).unwrap();
        bytes.truncate(bytes.len() - 16);
        std::fs::write(&archive_path, &bytes).unwrap();

        let result = extract_xz(&archive_path, &extracted_path, &crate::NoOpProgress).await;
        assert!(matches!(result, Err(Error::ExtractionFailed(_))));

        // Both the corrupt archive and the partial output are cleaned up so the
        // next attempt re-downloads instead of failing on the same bad cache.
        assert!(!archive_path.exists());
        assert!(!extracted_path.exists());
    }

    #[tokio::test]
    #[serial]
    async fn test_extract_xz_with_progress() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        struct TestProgressCallback {
            calls: Arc<Mutex<Vec<FlashProgress>>>,
        }

        impl crate::ProgressCallback for TestProgressCallback {
            fn on_progress(&self, progress: FlashProgress) {
                self.calls.lock().unwrap().push(progress);
            }
        }

        let cache_dir = get_cache_dir().unwrap();
        // Create larger content to trigger progress updates (> 10MB)
        let test_content = vec![0u8; 11 * 1024 * 1024]; // 11MB
        let extracted_path = cache_dir.join("test_extracted_large.img");
        let archive_path = cache_dir.join("test_archive_large.img.xz");

        // Create XZ compressed file
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut encoder = xz2::write::XzEncoder::new(file, 1); // Use compression level 1 for speed
            encoder.write_all(&test_content).unwrap();
            encoder.finish().unwrap();
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let callback = TestProgressCallback {
            calls: calls.clone(),
        };

        // Extract with progress tracking
        let result = extract_xz(&archive_path, &extracted_path, &callback).await;
        assert!(result.is_ok());

        // Verify we got progress callbacks
        let progress_calls = calls.lock().unwrap();
        assert!(!progress_calls.is_empty());
        assert!(progress_calls.iter().any(|p| p.progress == 0)); // Start
        assert!(progress_calls.iter().any(|p| p.progress == 100)); // End
        assert!(progress_calls
            .iter()
            .all(|p| p.stage == FlashStage::Extracting));

        // Cleanup
        std::fs::remove_file(&archive_path).unwrap();
        std::fs::remove_file(&extracted_path).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_is_cached_with_matching_size() {
        std::env::remove_var("HA_INSTALLER_NO_CACHE");

        let cache_dir = get_cache_dir().unwrap();
        let test_file = cache_dir.join("test_matching_size.img.xz");

        // Write exactly 100 bytes
        std::fs::write(&test_file, [0u8; 100]).unwrap();

        // Image expects exactly 100 bytes
        let image = HaosImage {
            board: "test".to_string(),
            format: ImageFormat::Raw,
            download_url: format!(
                "https://example.com/{}",
                test_file.file_name().unwrap().to_string_lossy()
            ),
            size: 100,
        };

        let result = is_cached(&image).await.unwrap();
        assert!(result, "Should return true when file size matches");

        // Cleanup
        std::fs::remove_file(&test_file).unwrap();
    }

    #[tokio::test]
    async fn test_cleanup_cache_nonexistent_directory() {
        // This tests the early return path when the cache directory doesn't exist
        // The function should handle this gracefully
        let result = cleanup_cache().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_parse_github_release_invalid_filename_skipped() {
        use crate::types::{GitHubAsset, GitHubRelease};

        let release = GitHubRelease {
            tag_name: "14.2".to_string(),
            assets: vec![GitHubAsset {
                name: "invalid_filename.img.xz".to_string(), // Doesn't match pattern
                size: 500_000_000,
                browser_download_url: "https://github.com/download/invalid.img.xz".to_string(),
            }],
        };

        let parsed = parse_github_release(release).unwrap();
        assert_eq!(parsed.images.len(), 0); // Invalid filename should be skipped
    }

    #[tokio::test]
    async fn test_parse_board_from_filename_with_suffix_error() {
        // Test the error path in parse_board_from_filename_with_suffix
        let result = parse_board_from_filename_with_suffix(
            "haos_rpi4-14.2.img.xz",
            "99.9", // Wrong version
            ".img.xz",
        );
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(matches!(e, crate::error::Error::InvalidConfig(_)));
        }
    }

    #[tokio::test]
    async fn test_fetch_release_network_error() {
        let mut server = mockito::Server::new_async().await;

        let _mock = server
            .mock("GET", "/tags/14.2")
            .with_status(404)
            .create_async()
            .await;

        // Can't easily test this without dependency injection
        // This test documents the intent
    }

    // HTTP Mock Tests Module
    // These tests use mockito to mock external HTTP endpoints
    mod http_mock_tests {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_success() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .match_header("User-Agent", USER_AGENT)
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(r#"{"hassos":{"rpi4":"14.2","generic-x86-64":"14.2"}}"#)
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            assert!(result.is_ok());

            let version_info = result.unwrap();
            assert_eq!(version_info.hassos.get("rpi4"), Some(&"14.2".to_string()));
            assert_eq!(
                version_info.hassos.get("generic-x86-64"),
                Some(&"14.2".to_string())
            );

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_http_404() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .with_status(404)
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            assert!(result.is_err());

            if let Err(e) = result {
                assert!(matches!(e, crate::error::Error::DownloadFailed(_)));
            }

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_http_500() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .with_status(500)
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            assert!(result.is_err());

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_invalid_json() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body("not valid json")
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            assert!(result.is_err());

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_empty_response() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body("")
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            assert!(result.is_err());

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_malformed_json() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(r#"{"hassos":{"rpi4":"14.2"}}"#) // Missing expected fields, but valid JSON
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            // This should succeed since the JSON is valid, even if minimal
            assert!(result.is_ok());

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_success() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .match_header("User-Agent", USER_AGENT)
                .match_header("Accept", "application/vnd.github.v3+json")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{
                    "tag_name": "14.2",
                    "assets": [
                        {
                            "name": "haos_rpi5-64-14.2.img.xz",
                            "size": 500000000,
                            "browser_download_url": "https://github.com/download/rpi5.img.xz",
                            "digest": "sha256:abc123"
                        },
                        {
                            "name": "haos_generic-x86-64-14.2.qcow2.xz",
                            "size": 600000000,
                            "browser_download_url": "https://github.com/download/x86.qcow2.xz",
                            "digest": "sha256:def456"
                        }
                    ]
                }"#,
                )
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_ok());

            let release = result.unwrap();
            assert_eq!(release.version, "14.2");
            assert_eq!(release.images.len(), 2);
            assert!(release.images.iter().any(|i| i.board == "rpi5-64"));
            assert!(release.images.iter().any(|i| i.board == "generic-x86-64"));

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_http_404() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/99.99")
                .with_status(404)
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "99.99").await;
            assert!(result.is_err());

            if let Err(e) = result {
                assert!(matches!(e, crate::error::Error::DownloadFailed(_)));
                let error_msg = format!("{:?}", e);
                assert!(error_msg.contains("404"));
            }

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_http_500() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .with_status(500)
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_err());

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_invalid_json() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body("invalid json")
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_err());

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_empty_assets() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{
                    "tag_name": "14.2",
                    "assets": []
                }"#,
                )
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_ok());

            let release = result.unwrap();
            assert_eq!(release.version, "14.2");
            assert_eq!(release.images.len(), 0);

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_mixed_assets() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{
                    "tag_name": "14.2",
                    "assets": [
                        {
                            "name": "haos_rpi5-64-14.2.img.xz",
                            "size": 500000000,
                            "browser_download_url": "https://github.com/download/rpi5.img.xz",
                            "digest": "sha256:abc123"
                        },
                        {
                            "name": "haos_rpi5-64-14.2.img.xz.sha256",
                            "size": 100,
                            "browser_download_url": "https://github.com/download/sha256",
                            "digest": null
                        },
                        {
                            "name": "README.md",
                            "size": 1000,
                            "browser_download_url": "https://github.com/download/readme",
                            "digest": null
                        }
                    ]
                }"#,
                )
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_ok());

            let release = result.unwrap();
            assert_eq!(release.version, "14.2");
            assert_eq!(release.images.len(), 1); // Only valid image files
            assert_eq!(release.images[0].board, "rpi5-64");

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_with_redirects() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .with_status(302)
                .with_header("Location", "/redirected/tags/14.2")
                .create_async()
                .await;

            let redirect_mock = server
                .mock("GET", "/redirected/tags/14.2")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{
                    "tag_name": "14.2",
                    "assets": [{
                        "name": "haos_rpi4-14.2.img.xz",
                        "size": 400000000,
                        "browser_download_url": "https://github.com/download/rpi4.img.xz",
                        "digest": "sha256:xyz789"
                    }]
                }"#,
                )
                .create_async()
                .await;

            // reqwest follows redirects by default
            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_ok());

            let release = result.unwrap();
            assert_eq!(release.version, "14.2");
            assert_eq!(release.images.len(), 1);

            mock.assert_async().await;
            redirect_mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_download_image_empty_response() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/empty.img.xz")
                .with_status(200)
                .with_header("content-length", "0")
                .with_body("")
                .create_async()
                .await;

            let url = format!("{}/empty.img.xz", server.url());
            let cache_dir = get_cache_dir().unwrap();
            let dest = cache_dir.join("test_empty.img");

            let result = download_image(&url, &dest, &crate::NoOpProgress).await;
            assert!(result.is_ok());

            // Verify empty file was created
            let metadata = std::fs::metadata(&dest).unwrap();
            assert_eq!(metadata.len(), 0);

            mock.assert_async().await;
            std::fs::remove_file(&dest).unwrap();
        }

        #[tokio::test]
        #[serial]
        async fn test_download_image_with_redirect() {
            let mut server = mockito::Server::new_async().await;

            let redirect_mock = server
                .mock("GET", "/redirect.img.xz")
                .with_status(302)
                .with_header("Location", "/actual.img.xz")
                .create_async()
                .await;

            let test_data = b"redirected content";
            let actual_mock = server
                .mock("GET", "/actual.img.xz")
                .with_status(200)
                .with_header("content-length", &test_data.len().to_string())
                .with_body(test_data.as_slice())
                .create_async()
                .await;

            let url = format!("{}/redirect.img.xz", server.url());
            let cache_dir = get_cache_dir().unwrap();
            let dest = cache_dir.join("test_redirect.img");

            let result = download_image(&url, &dest, &crate::NoOpProgress).await;
            assert!(result.is_ok());

            // Verify file content
            let content = std::fs::read(&dest).unwrap();
            assert_eq!(content, test_data);

            redirect_mock.assert_async().await;
            actual_mock.assert_async().await;
            std::fs::remove_file(&dest).unwrap();
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_with_extra_fields() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/stable.json")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{
                    "hassos": {
                        "rpi4": "14.2",
                        "generic-x86-64": "14.2",
                        "green": "14.2",
                        "yellow": "14.2"
                    },
                    "extra_field": "should be ignored"
                }"#,
                )
                .create_async()
                .await;

            let url = format!("{}/stable.json", server.url());
            let result = get_stable_version_from_url(&url).await;
            assert!(result.is_ok());

            let version_info = result.unwrap();
            assert_eq!(version_info.hassos.len(), 4);
            assert_eq!(version_info.hassos.get("green"), Some(&"14.2".to_string()));

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_with_qcow2_only() {
            let mut server = mockito::Server::new_async().await;

            let mock = server
                .mock("GET", "/tags/14.2")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{
                    "tag_name": "14.2",
                    "assets": [
                        {
                            "name": "haos_generic-x86-64-14.2.qcow2.xz",
                            "size": 600000000,
                            "browser_download_url": "https://github.com/download/x86.qcow2.xz",
                            "digest": "sha256:qcow2hash"
                        }
                    ]
                }"#,
                )
                .create_async()
                .await;

            let result = fetch_release_from_api(&server.url(), "14.2").await;
            assert!(result.is_ok());

            let release = result.unwrap();
            assert_eq!(release.images.len(), 1);
            assert_eq!(release.images[0].board, "generic-x86-64");
            assert!(release.images[0].download_url.contains("qcow2"));

            mock.assert_async().await;
        }

        #[tokio::test]
        #[serial]
        async fn test_fetch_release_connection_refused() {
            // Use a port that's likely not in use
            let result = fetch_release_from_api("http://127.0.0.1:59999", "14.2").await;
            assert!(result.is_err());
        }

        #[tokio::test]
        #[serial]
        async fn test_get_stable_version_connection_refused() {
            let result = get_stable_version_from_url("http://127.0.0.1:59998/stable.json").await;
            assert!(result.is_err());
        }
    }
}

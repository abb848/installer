//! Tauri command wrappers for hai-core functionality
//!
//! This module provides Tauri IPC commands that wrap the hai-core library.
//! It handles the bridge between Tauri's Channel<T> and hai-core's ProgressCallback trait.

use crate::backend::Backend;
use crate::flash_state::FlashState;
use hai_core::download::TemporaryImage;
use hai_core::{
    BlockDevice, DeviceBackend, DeviceManifest, ExpectedDevice, FlashProgress, FlashRequest,
    FlashStage, HaosRelease, HostBackend, ImageFormat, ProgressCallback, ProxmoxBackend,
    ProxmoxCredentials, ProxmoxNode, ProxmoxSession, ProxmoxStorage, ProxmoxVmConfig,
    ProxmoxVmResult, ReleaseSource, SystemInfo, UtmBackend, VmStatusInfo,
};
use tauri::ipc::Channel;

#[derive(Default)]
pub struct PendingUtmImages(std::sync::Mutex<std::collections::HashMap<String, TemporaryImage>>);

impl PendingUtmImages {
    fn take(&self, image_path: &str) -> Option<TemporaryImage> {
        self.0.lock().unwrap().remove(image_path)
    }
}

// =============================================================================
// Tauri Progress Callback Adapter
// =============================================================================

/// Adapter that bridges Tauri's Channel with hai-core's ProgressCallback trait
struct TauriProgressCallback<'a> {
    channel: &'a Channel<FlashProgress>,
}

impl<'a> TauriProgressCallback<'a> {
    fn new(channel: &'a Channel<FlashProgress>) -> Self {
        Self { channel }
    }
}

impl<'a> ProgressCallback for TauriProgressCallback<'a> {
    fn on_progress(&self, progress: FlashProgress) {
        let _ = self.channel.send(progress);
    }
}

// =============================================================================
// Request/Response Types
// =============================================================================

/// Result of a flash operation
#[derive(Debug, serde::Serialize)]
pub struct FlashResult {
    pub success: bool,
    pub error: Option<String>,
    pub duration_secs: u64,
}

// =============================================================================
// Device Commands
// =============================================================================

/// List all block devices
#[tauri::command]
pub async fn list_block_devices() -> Result<Vec<BlockDevice>, String> {
    Backend.list_devices().await.map_err(|e| e.to_string())
}

// =============================================================================
// Flash Commands
// =============================================================================

/// Find the flash target among the currently attached devices.
///
/// `write_image` writes to whatever target it is given, so this lookup is the
/// safety gate: the device must be one that enumeration reported, and it must
/// be removable, and it must still match the drive the user selected, since
/// the path can be reassigned while the image downloads.
fn find_flash_target<'a>(
    devices: &'a [BlockDevice],
    device_id: &str,
    expected: &ExpectedDevice,
) -> Result<&'a BlockDevice, String> {
    let device = devices.iter().find(|d| d.id == device_id).ok_or_else(|| {
        format!(
            "Device {} not found. It may have been disconnected.",
            device_id
        )
    })?;

    if !device.removable {
        return Err(format!(
            "{} is not a removable drive and cannot be overwritten",
            device_id
        ));
    }

    if !expected.matches(device) {
        return Err(format!(
            "The drive at {} is no longer the one you selected. It may have been \
             swapped for another device; please select your drive again.",
            device_id
        ));
    }

    Ok(device)
}

/// Flash an image to a device
#[tauri::command]
pub async fn flash_image(
    request: FlashRequest,
    progress_channel: Channel<FlashProgress>,
    state: tauri::State<'_, FlashState>,
) -> Result<FlashResult, String> {
    state
        .run(async move {
            let callback = TauriProgressCallback::new(&progress_channel);
            run_flash(&Backend, &request, &callback).await
        })
        .await
}

/// The message for a failed write, as the frontend shows it.
fn write_error_message(err: hai_core::Error) -> String {
    match err {
        // Verify-phase failures are tagged VerificationFailed; the rest are writes.
        hai_core::Error::VerificationFailed(msg) => format!("Verification failed: {}", msg),
        // Already carries its own "Disk service unavailable:" prefix.
        err @ hai_core::Error::DiskServiceUnavailable(_) => err.to_string(),
        // A disconnect doesn't require a prefix
        err @ hai_core::Error::DriveDisconnected => err.to_string(),
        // Self-explanatory messages; a "Write failed:" prefix would bury them.
        err @ hai_core::Error::WriteProtected => err.to_string(),
        err @ hai_core::Error::ImageTooLarge { .. } => err.to_string(),
        err @ hai_core::Error::Cancelled => err.to_string(),
        hai_core::Error::PermissionDenied(msg) => msg,
        other => format!("Write failed: {}", other),
    }
}

/// Download, extract, and write the image for `request`.
///
/// Generic over the backend so the whole flow can be exercised against
/// `BackendMock`.
async fn run_flash<B, P>(
    backend: &B,
    request: &FlashRequest,
    callback: &P,
) -> Result<FlashResult, String>
where
    B: ReleaseSource + DeviceBackend,
    P: ProgressCallback,
{
    let start_time = std::time::Instant::now();

    backend
        .check_write_privileges()
        .map_err(|e| e.to_string())?;

    // Send initial progress
    callback.on_progress(FlashProgress {
        stage: FlashStage::Downloading,
        progress: 0,
        bytes_processed: 0,
        total_bytes: 0,
        message: "Fetching release info...".to_string(),
    });

    // Fetch the release this board is on
    let release = backend
        .get_latest_haos_release_for_board(&request.board)
        .await
        .map_err(|e| format!("Failed to fetch release info: {}", e))?;

    // Find the raw disk image for the requested board. Some boards also ship a
    // qcow2 under the same board name, which must never be written to a drive.
    let image = release
        .image_for(&request.board, ImageFormat::Raw)
        .ok_or_else(|| format!("No image found for board: {}", request.board))?;

    callback.on_progress(FlashProgress {
        stage: FlashStage::Downloading,
        progress: 0,
        bytes_processed: 0,
        total_bytes: image.size,
        message: "Starting download...".to_string(),
    });

    // Get cache directory and download
    let cache_dir = backend
        .cache_dir()
        .map_err(|e| format!("Cache error: {}", e))?;
    let temporary_image =
        TemporaryImage::new(&cache_dir, ImageFormat::Raw).map_err(|e| e.to_string())?;
    let compressed_path = temporary_image.archive_path();

    backend
        .download_image(image, &compressed_path, callback)
        .await
        .map_err(|e| e.to_string())?;

    // Extract the image
    let extracted_path = temporary_image.path();

    backend
        .extract_temporary_image(&temporary_image, callback)
        .await
        .map_err(|e| e.to_string())?;
    temporary_image.cache_archive(&cache_dir, &image.board, &release.version);

    // Check image size vs device size
    let image_size = tokio::fs::metadata(&extracted_path)
        .await
        .map_err(|e| format!("Failed to get image size: {}", e))?
        .len();

    let device_list = backend
        .list_devices()
        .await
        .map_err(|e| format!("Failed to list devices: {}", e))?;

    let device = find_flash_target(&device_list, &request.device_id, &request.expected_device)?;

    if image_size > device.size {
        return Err(format!(
            "Image is too large for the selected device. Image size: {:.1} GB, Device size: {:.1} GB.",
            image_size as f64 / 1_000_000_000.0,
            device.size as f64 / 1_000_000_000.0
        ));
    }

    // Write to device
    backend
        .write_image(
            &extracted_path,
            &request.device_id,
            request.verify,
            callback,
        )
        .await
        .map_err(write_error_message)?;

    drop(temporary_image);

    let duration = start_time.elapsed();

    callback.on_progress(FlashProgress {
        stage: FlashStage::Complete,
        progress: 100,
        bytes_processed: 0,
        total_bytes: 0,
        message: "Installation complete!".to_string(),
    });

    Ok(FlashResult {
        success: true,
        error: None,
        duration_secs: duration.as_secs(),
    })
}

// =============================================================================
// Release/Manifest Commands
// =============================================================================

/// Get the latest HAOS release information
#[tauri::command]
pub async fn get_haos_release(version: Option<String>) -> Result<HaosRelease, String> {
    let ver = version.as_deref().unwrap_or("latest");
    Backend
        .get_haos_release(ver)
        .await
        .map_err(|e| e.to_string())
}

/// Get the device manifest
#[tauri::command]
pub async fn get_manifest() -> Result<DeviceManifest, String> {
    Backend
        .get_device_manifest()
        .await
        .map_err(|e| e.to_string())
}

// =============================================================================
// System Info Commands
// =============================================================================

/// Get system information (CPU cores and memory) for VM configuration limits
#[tauri::command]
pub fn get_system_info() -> Result<SystemInfo, String> {
    Backend.system_info().map_err(|e| e.to_string())
}

// =============================================================================
// UTM Commands (macOS only)
// =============================================================================

/// Download the HAOS qcow2 image for UTM
#[tauri::command]
pub async fn download_utm_image(
    progress_channel: Channel<FlashProgress>,
    pending: tauri::State<'_, PendingUtmImages>,
) -> Result<String, String> {
    let callback = TauriProgressCallback::new(&progress_channel);

    // Verify UTM is available before doing any work.
    Backend
        .check_utm_status()
        .await
        .map_err(|e| e.to_string())?;
    let arch = if cfg!(target_arch = "aarch64") {
        "generic-aarch64"
    } else {
        "generic-x86-64"
    };

    let image = run_utm_download(&Backend, arch, &callback).await?;
    let path = image.path().to_string_lossy().into_owned();
    pending.0.lock().unwrap().insert(path.clone(), image);
    Ok(path)
}

/// Download and extract the HAOS qcow2 image for `arch`, returning the extracted path.
///
/// Generic over the backend so it can be exercised against `BackendMock`.
async fn run_utm_download<B, P>(
    backend: &B,
    arch: &str,
    callback: &P,
) -> Result<TemporaryImage, String>
where
    B: ReleaseSource,
    P: ProgressCallback,
{
    callback.on_progress(FlashProgress {
        stage: FlashStage::Downloading,
        progress: 0,
        bytes_processed: 0,
        total_bytes: 0,
        message: "Fetching release info...".to_string(),
    });

    let release = backend
        .get_latest_haos_release_for_board(arch)
        .await
        .map_err(|e| format!("Failed to fetch release: {}", e))?;

    let image = release
        .image_for(arch, ImageFormat::Qcow2)
        .ok_or_else(|| format!("No qcow2 image found for: {}", arch))?;

    let cache_dir = backend.cache_dir().map_err(|e| e.to_string())?;
    let temporary_image =
        TemporaryImage::new(&cache_dir, ImageFormat::Qcow2).map_err(|e| e.to_string())?;
    let compressed_path = temporary_image.archive_path();

    backend
        .download_image(image, &compressed_path, callback)
        .await
        .map_err(|e| e.to_string())?;

    backend
        .extract_temporary_image(&temporary_image, callback)
        .await
        .map_err(|e| e.to_string())?;
    temporary_image.cache_archive(&cache_dir, &image.board, &release.version);

    callback.on_progress(FlashProgress {
        stage: FlashStage::Complete,
        progress: 100,
        bytes_processed: 0,
        total_bytes: 0,
        message: "Download complete!".to_string(),
    });

    Ok(temporary_image)
}

/// Release a downloaded image abandoned before VM creation. Unknown paths are
/// never deleted, and an image being imported has already left this registry.
#[tauri::command]
pub fn discard_utm_image(image_path: String, pending: tauri::State<'_, PendingUtmImages>) {
    pending.take(&image_path);
}

/// Check if UTM is installed and get its status
#[tauri::command]
pub async fn check_utm_status() -> Result<hai_core::UtmStatus, String> {
    Backend.check_utm_status().await.map_err(|e| e.to_string())
}

/// Create a Home Assistant VM in UTM
#[tauri::command]
pub async fn create_utm_vm(
    config: hai_core::UtmVmConfig,
    pending: tauri::State<'_, PendingUtmImages>,
) -> Result<String, String> {
    run_utm_creation(&Backend, &config, &pending).await
}

async fn run_utm_creation<B: UtmBackend>(
    backend: &B,
    config: &hai_core::UtmVmConfig,
    pending: &PendingUtmImages,
) -> Result<String, String> {
    let image = pending
        .take(&config.image_path)
        .ok_or_else(|| "Temporary image is no longer available; download it again".to_string())?;
    image.begin_utm_import().map_err(|error| {
        format!(
            "Could not prepare the UTM import: {error}. UTM was not contacted. \
         Any retained source directory at {} can be removed manually.",
            image.path().parent().unwrap().display()
        )
    })?;
    // Fully qualified: `create_vm` is defined on both UtmBackend and ProxmoxBackend.
    let result = UtmBackend::create_vm(backend, config, &hai_core::NoOpProgress).await;
    if let Err(hai_core::Error::UtmOperationUncertain(error)) = &result {
        return Err(format!(
            "UTM may still be importing the image: {error}. Source retained at {}. \
             Check UTM before retrying. Remove the source directory manually only after \
             confirming UTM has finished or stopped importing.",
            image.path().parent().unwrap().display()
        ));
    }
    // A completed creation or confirmed rejection no longer needs the source.
    if let Err(error) = image.finish_utm_import() {
        eprintln!(
            "Could not release UTM source {}: {error}",
            image.path().display()
        );
    }
    result.map(|result| result.id).map_err(|e| e.to_string())
}

/// Start a UTM VM
#[tauri::command(async)]
pub fn start_utm_vm(vm_id: String) -> Result<(), String> {
    Backend.start_vm(&vm_id).map_err(|e| e.to_string())
}

/// Resize a UTM VM's disk
#[tauri::command]
pub fn resize_utm_vm_disk(vm_id: String, size_gb: u32) -> Result<(), String> {
    Backend
        .resize_vm_disk(&vm_id, size_gb)
        .map_err(|e| e.to_string())
}

/// Get the status of a UTM VM
#[tauri::command(async)]
pub fn get_utm_vm_status(vm_id: String) -> Result<VmStatusInfo, String> {
    Backend.vm_status(&vm_id).map_err(|e| e.to_string())
}

// =============================================================================
// HA Status Commands
// =============================================================================

/// Check if Home Assistant webserver is ready
#[tauri::command]
pub async fn check_ha_ready(ip_address: String) -> bool {
    Backend.check_ha_ready(&ip_address).await
}

/// Check if Home Assistant has finished updating
#[tauri::command]
pub async fn check_ha_updated(ip_address: String) -> bool {
    Backend.check_ha_updated(&ip_address).await
}

// =============================================================================
// Proxmox Commands
// =============================================================================

/// Connect to a Proxmox VE server
#[tauri::command]
pub async fn proxmox_connect(credentials: ProxmoxCredentials) -> Result<ProxmoxSession, String> {
    Backend
        .authenticate(&credentials)
        .await
        .map_err(|e| e.to_string())
}

/// List available nodes on Proxmox
#[tauri::command]
pub async fn proxmox_list_nodes(session: ProxmoxSession) -> Result<Vec<ProxmoxNode>, String> {
    Backend
        .list_nodes(&session)
        .await
        .map_err(|e| e.to_string())
}

/// List available storage on a Proxmox node
#[tauri::command]
pub async fn proxmox_list_storage(
    session: ProxmoxSession,
    node: String,
) -> Result<Vec<ProxmoxStorage>, String> {
    Backend
        .list_storage(&session, &node)
        .await
        .map_err(|e| e.to_string())
}

/// Get the next available VM ID on Proxmox
#[tauri::command]
pub async fn proxmox_get_next_vm_id(session: ProxmoxSession) -> Result<u32, String> {
    Backend
        .get_next_vm_id(&session)
        .await
        .map_err(|e| e.to_string())
}

/// Create a Home Assistant VM on Proxmox
#[tauri::command]
pub async fn proxmox_create_vm(
    session: ProxmoxSession,
    config: ProxmoxVmConfig,
    progress_channel: Channel<FlashProgress>,
) -> Result<ProxmoxVmResult, String> {
    let callback = TauriProgressCallback::new(&progress_channel);
    // Fully qualified: `create_vm` is defined on both ProxmoxBackend and UtmBackend.
    ProxmoxBackend::create_vm(&Backend, &session, &config, &callback)
        .await
        .map_err(|e| e.to_string())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_error_message_shows_write_protection_without_a_prefix() {
        let msg = write_error_message(hai_core::Error::WriteProtected);
        assert_eq!(msg, hai_core::Error::WriteProtected.to_string());
        assert!(!msg.starts_with("Write failed"), "{msg}");
    }

    #[test]
    fn write_error_message_prefixes_a_plain_io_error() {
        let msg = write_error_message(hai_core::Error::Io(std::io::Error::other("boom")));
        assert!(msg.starts_with("Write failed"), "{msg}");
    }

    // ===== Manifest Tests =====

    #[tokio::test]
    async fn test_get_manifest_returns_ok() {
        let result = get_manifest().await;
        assert!(result.is_ok());
        let manifest = result.unwrap();
        assert!(!manifest.devices.is_empty());
    }

    #[tokio::test]
    async fn test_get_manifest_has_devices() {
        let result = get_manifest().await;
        assert!(result.is_ok());
        let manifest = result.unwrap();
        assert!(!manifest.devices.is_empty());
        assert!(manifest.version > 0);
    }

    #[tokio::test]
    async fn test_get_manifest_devices_have_valid_haos_config() {
        let result = get_manifest().await;
        assert!(result.is_ok());
        let manifest = result.unwrap();
        for device in manifest.devices {
            assert!(!device.id.is_empty());
            assert!(!device.name.is_empty());
            assert!(!device.haos.board.is_empty());
            assert!(!device.haos.download_url.is_empty());
            // Verify URL template contains placeholder
            assert!(device.haos.download_url.contains("{version}"));
        }
    }

    // ===== Non-mock System Info Tests =====

    #[cfg(not(feature = "mock"))] // asserts on the real backend's answers
    #[test]
    #[cfg(target_os = "macos")]
    fn test_system_info_macos_fallback_on_error() {
        let info = get_system_info().unwrap();
        // Real values from sysctl, or the fallback defaults if it fails.
        // Either way they're non-zero; CI runners can have fewer than 4 cores.
        assert!(info.cpu_cores >= 1);
        assert!(info.memory_mb > 0);
    }

    #[cfg(not(feature = "mock"))] // asserts on the real backend's answers
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn test_system_info_non_macos() {
        assert!(get_system_info().is_err());
    }

    // ===== Additional edge case tests =====

    #[test]
    #[cfg(feature = "mock")]
    fn test_start_utm_vm_mock_returns_ok() {
        let result = start_utm_vm("test-vm".to_string());
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_resize_utm_vm_disk_non_mock_returns_ok() {
        let result = resize_utm_vm_disk("test-vm".to_string(), 64);
        // Should return Ok even though not implemented
        assert!(result.is_ok());
    }

    // Off macOS the commands no longer gate themselves; hai-core has to
    // refuse them, and that refusal has to reach the frontend.
    #[cfg(not(feature = "mock"))] // asserts on the real backend's answers
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn test_start_utm_vm_unsupported_off_macos() {
        let err = start_utm_vm("test-vm".to_string()).unwrap_err();
        assert!(err.contains("only available on macOS"), "{err}");
    }

    #[cfg(not(feature = "mock"))] // asserts on the real backend's answers
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn test_resize_utm_vm_disk_unsupported_off_macos() {
        let err = resize_utm_vm_disk("test-vm".to_string(), 64).unwrap_err();
        assert!(err.contains("only available on macOS"), "{err}");
    }

    #[test]
    #[cfg(feature = "mock")]
    fn test_get_utm_vm_status_mock_returns_running_vm() {
        let result = get_utm_vm_status("test-vm".to_string());
        assert!(result.is_ok());
        let status = result.unwrap();
        assert_eq!(status.status, "started");
        assert_eq!(status.ip_address.as_deref(), Some("192.168.1.100"));
    }

    // ===== check_ha_ready() Tests =====

    #[cfg(not(feature = "mock"))] // asserts on the real backend's answers
    #[tokio::test]
    async fn test_check_ha_ready_empty_ip() {
        let result = check_ha_ready("".to_string()).await;
        assert!(!result, "Should return false for empty IP");
    }

    // ===== Additional Edge Cases =====

    fn flash_target(id: &str, removable: bool) -> BlockDevice {
        BlockDevice {
            id: id.to_string(),
            name: "Test Device".to_string(),
            size: 32_000_000_000,
            device_type: hai_core::DeviceType::UsbDrive,
            removable,
            model: None,
            vendor: None,
        }
    }

    /// The identity the frontend sends for a device built by `flash_target`.
    fn expected() -> ExpectedDevice {
        ExpectedDevice {
            size: Some(32_000_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn test_find_flash_target_accepts_removable_device() {
        let devices = [flash_target("/dev/sdb", true)];
        let device = find_flash_target(&devices, "/dev/sdb", &expected()).unwrap();
        assert_eq!(device.id, "/dev/sdb");
    }

    #[test]
    fn test_find_flash_target_rejects_unknown_device() {
        let devices = [flash_target("/dev/sdb", true)];
        let err = find_flash_target(&devices, "/dev/sdz", &expected()).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn test_find_flash_target_rejects_non_removable_device() {
        let devices = [flash_target("\\\\.\\PhysicalDrive1", false)];
        let err = find_flash_target(&devices, "\\\\.\\PhysicalDrive1", &expected()).unwrap_err();
        assert!(err.contains("not a removable drive"), "{err}");
    }

    #[test]
    fn test_find_flash_target_rejects_empty_device_id() {
        let devices = [flash_target("/dev/sdb", true)];
        let err = find_flash_target(&devices, "", &expected()).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn test_find_flash_target_rejects_different_device_at_same_path() {
        let mut device = flash_target("/dev/sdb", true);
        device.model = Some("Extreme".to_string());
        let err = find_flash_target(&[device], "/dev/sdb", &expected()).unwrap_err();
        assert!(err.contains("no longer the one you selected"), "{err}");
    }

    #[test]
    fn test_find_flash_target_rejects_unknown_expected_size() {
        let devices = [flash_target("/dev/sdb", true)];
        let unknown = ExpectedDevice::default();
        assert!(find_flash_target(&devices, "/dev/sdb", &unknown).is_err());
    }

    struct PreflightBackend {
        check: fn() -> hai_core::Result<()>,
    }

    impl DeviceBackend for PreflightBackend {
        fn check_write_privileges(&self) -> hai_core::Result<()> {
            (self.check)()
        }

        async fn list_devices(&self) -> hai_core::Result<Vec<BlockDevice>> {
            panic!("must not enumerate devices");
        }

        async fn write_image<P: ProgressCallback>(
            &self,
            _: &std::path::Path,
            _: &str,
            _: bool,
            _: &P,
        ) -> hai_core::Result<()> {
            panic!("must not write a drive");
        }
    }

    impl ReleaseSource for PreflightBackend {
        async fn get_device_manifest(&self) -> hai_core::Result<DeviceManifest> {
            panic!("must not fetch a manifest");
        }

        async fn get_haos_release(&self, _: &str) -> hai_core::Result<HaosRelease> {
            panic!("must not fetch a release by version");
        }

        async fn get_latest_haos_release_for_board(
            &self,
            _: &str,
        ) -> hai_core::Result<HaosRelease> {
            Err(hai_core::Error::InvalidConfig(
                "release lookup reached".into(),
            ))
        }

        async fn download_image<P: ProgressCallback>(
            &self,
            _: &hai_core::HaosImage,
            _: &std::path::Path,
            _: &P,
        ) -> hai_core::Result<()> {
            panic!("must not download an image");
        }

        async fn extract_xz<P: ProgressCallback>(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
            _: &P,
        ) -> hai_core::Result<()> {
            panic!("must not extract an image");
        }

        fn cache_dir(&self) -> hai_core::Result<std::path::PathBuf> {
            panic!("must not create a cache directory");
        }
    }

    struct NoProgressExpected;

    impl ProgressCallback for NoProgressExpected {
        fn on_progress(&self, _: FlashProgress) {
            panic!("must not report progress before privilege preflight succeeds");
        }
    }

    #[tokio::test]
    async fn privilege_failures_abort_before_progress_or_release_lookup() {
        fn not_elevated() -> hai_core::Result<()> {
            Err(hai_core::Error::PermissionDenied(
                "Run as administrator".into(),
            ))
        }
        fn query_failed() -> hai_core::Result<()> {
            Err(hai_core::Error::Io(std::io::Error::other(
                "token query failed",
            )))
        }
        async fn attempt(
            check: fn() -> hai_core::Result<()>,
            callback: &impl ProgressCallback,
        ) -> Result<FlashResult, String> {
            let request = FlashRequest {
                device_id: "unused-device".into(),
                board: "rpi5-64".into(),
                verify: true,
                expected_device: ExpectedDevice::default(),
            };
            run_flash(&PreflightBackend { check }, &request, callback).await
        }

        for check in [not_elevated, query_failed] {
            let err = attempt(check, &NoProgressExpected).await.unwrap_err();
            assert_eq!(err, check().unwrap_err().to_string());
        }

        // An authorized attempt reaches release lookup without real network I/O.
        let retry = attempt(|| Ok(()), &hai_core::NoOpProgress)
            .await
            .unwrap_err();
        assert!(retry.contains("release lookup reached"), "{retry}");
    }
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests {
    use super::*;
    use hai_core::{BackendMock, NoOpProgress};
    use serial_test::serial;

    struct LifecycleBackend {
        cache: tempfile::TempDir,
        outcome: &'static str,
        extracted: std::sync::Mutex<Option<std::path::PathBuf>>,
        write_started: tokio::sync::Notify,
    }

    impl ReleaseSource for LifecycleBackend {
        async fn get_device_manifest(&self) -> hai_core::Result<DeviceManifest> {
            BackendMock.get_device_manifest().await
        }
        async fn get_haos_release(&self, version: &str) -> hai_core::Result<HaosRelease> {
            BackendMock.get_haos_release(version).await
        }
        async fn get_latest_haos_release_for_board(
            &self,
            board: &str,
        ) -> hai_core::Result<HaosRelease> {
            BackendMock.get_latest_haos_release_for_board(board).await
        }
        async fn download_image<P: ProgressCallback>(
            &self,
            _: &hai_core::HaosImage,
            dest: &std::path::Path,
            _: &P,
        ) -> hai_core::Result<()> {
            std::fs::write(dest, b"archive")?;
            Ok(())
        }
        async fn extract_xz<P: ProgressCallback>(
            &self,
            _: &std::path::Path,
            dest: &std::path::Path,
            _: &P,
        ) -> hai_core::Result<()> {
            *self.extracted.lock().unwrap() = Some(dest.to_path_buf());
            std::fs::write(dest, b"image")?;
            if self.outcome == "extract" {
                return Err(hai_core::Error::ExtractionFailed(
                    "test extraction failure".into(),
                ));
            }
            Ok(())
        }
        fn cache_dir(&self) -> hai_core::Result<std::path::PathBuf> {
            Ok(self.cache.path().to_path_buf())
        }
    }

    impl DeviceBackend for LifecycleBackend {
        async fn list_devices(&self) -> hai_core::Result<Vec<BlockDevice>> {
            BackendMock.list_devices().await
        }
        async fn write_image<P: ProgressCallback>(
            &self,
            path: &std::path::Path,
            _: &str,
            _: bool,
            _: &P,
        ) -> hai_core::Result<()> {
            assert_eq!(std::fs::read(path).unwrap(), b"image");
            match self.outcome {
                "write" => Err(hai_core::Error::Io(std::io::Error::other(
                    "test write failure",
                ))),
                "cancel" => Err(hai_core::Error::Cancelled),
                "suspend" => {
                    self.write_started.notify_one();
                    std::future::pending().await
                }
                _ => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn flash_releases_extraction_on_every_exit() {
        for outcome in ["success", "extract", "write", "cancel", "suspend"] {
            let backend = LifecycleBackend {
                cache: tempfile::tempdir().unwrap(),
                outcome,
                extracted: Default::default(),
                write_started: Default::default(),
            };
            let request = request("mock-sd-card-32gb", "rpi5-64").await;
            let result = if outcome == "suspend" {
                let flash = run_flash(&backend, &request, &NoOpProgress);
                tokio::pin!(flash);
                tokio::select! {
                    _ = backend.write_started.notified() => None,
                    result = &mut flash => Some(result),
                }
            } else {
                Some(run_flash(&backend, &request, &NoOpProgress).await)
            };
            match outcome {
                "success" => assert!(result.unwrap().unwrap().success),
                "suspend" => assert!(result.is_none()),
                "cancel" => assert!(result.unwrap().unwrap_err().contains("cancelled")),
                _ => assert!(result.unwrap().is_err()),
            }
            let path = backend.extracted.lock().unwrap().clone().unwrap();
            assert!(!path.exists(), "{outcome}");
            assert!(!path.parent().unwrap().exists(), "{outcome}");
            assert_eq!(
                backend
                    .cache
                    .path()
                    .join("haos_rpi5-64-16.3.img.xz")
                    .exists(),
                outcome != "extract"
            );
        }
    }

    #[tokio::test]
    async fn utm_download_publishes_only_completed_archives() {
        for outcome in ["success", "extract"] {
            let backend = LifecycleBackend {
                cache: tempfile::tempdir().unwrap(),
                outcome,
                extracted: Default::default(),
                write_started: Default::default(),
            };
            let result = run_utm_download(&backend, "generic-aarch64", &NoOpProgress).await;
            assert_eq!(result.is_ok(), outcome == "success");
            assert_eq!(
                backend
                    .cache
                    .path()
                    .join("haos_generic-aarch64-16.3.qcow2.xz")
                    .exists(),
                outcome == "success"
            );
            drop(result);
            let path = backend.extracted.lock().unwrap().clone().unwrap();
            assert!(!path.exists());
        }
    }

    #[test]
    fn utm_registry_never_deletes_unknown_or_in_use_images() {
        let cache = tempfile::tempdir().unwrap();
        let user_file = cache.path().join("user.qcow2");
        std::fs::write(&user_file, b"user image").unwrap();
        let pending = PendingUtmImages::default();
        assert!(pending.take(user_file.to_str().unwrap()).is_none());
        assert!(user_file.exists());
        let image = TemporaryImage::new(cache.path(), ImageFormat::Qcow2).unwrap();
        let path = image.path().to_string_lossy().into_owned();
        std::fs::write(&path, b"owned image").unwrap();
        pending.0.lock().unwrap().insert(path.clone(), image);
        let importing = pending.take(&path).unwrap();
        assert!(pending.take(&path).is_none());
        assert!(std::path::Path::new(&path).exists());
        drop(importing);
        assert!(!std::path::Path::new(&path).exists());
        assert!(user_file.exists());
    }

    struct UtmCreationBackend {
        outcome: &'static str,
        started: tokio::sync::Notify,
    }

    impl UtmBackend for UtmCreationBackend {
        async fn check_utm_status(&self) -> hai_core::Result<hai_core::UtmStatus> {
            unreachable!()
        }

        async fn create_vm<P: ProgressCallback>(
            &self,
            config: &hai_core::UtmVmConfig,
            _: &P,
        ) -> hai_core::Result<hai_core::UtmVmResult> {
            let source = std::path::Path::new(&config.image_path);
            assert!(source.exists());
            assert!(source.parent().unwrap().join(".utm-import").exists());
            self.started.notify_one();
            match self.outcome {
                "release-failure" => {
                    let marker = source.parent().unwrap().join(".utm-import");
                    std::fs::remove_file(&marker).unwrap();
                    std::fs::create_dir(marker).unwrap();
                    Ok(hai_core::UtmVmResult {
                        id: "stable-utm-id".into(),
                        name: config.name.clone(),
                        path: None,
                    })
                }
                "success" => Ok(hai_core::UtmVmResult {
                    id: "stable-utm-id".into(),
                    name: config.name.clone(),
                    path: None,
                }),
                "rejected" => Err(hai_core::Error::Utm("Invalid configuration".into())),
                "timeout" => Err(hai_core::Error::UtmOperationUncertain(
                    "AppleEvent timed out (-1712)".into(),
                )),
                "transport" => Err(hai_core::Error::UtmOperationUncertain(
                    "Failed to wait for AppleScript".into(),
                )),
                "cancelled" => std::future::pending().await,
                _ => unreachable!(),
            }
        }

        fn start_vm(&self, _: &str) -> hai_core::Result<()> {
            unreachable!()
        }
        fn resize_vm_disk(&self, _: &str, _: u32) -> hai_core::Result<()> {
            unreachable!()
        }
        fn vm_status(&self, _: &str) -> hai_core::Result<VmStatusInfo> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn utm_source_cleanup_requires_a_confirmed_outcome() {
        for outcome in [
            "success",
            "rejected",
            "timeout",
            "transport",
            "cancelled",
            "marker-failure",
            "release-failure",
        ] {
            let cache = tempfile::tempdir().unwrap();
            let image = TemporaryImage::new(cache.path(), ImageFormat::Qcow2).unwrap();
            let path = image.path();
            std::fs::write(&path, b"source image").unwrap();
            if outcome == "marker-failure" {
                std::fs::create_dir(path.parent().unwrap().join(".utm-import")).unwrap();
            }
            let pending = PendingUtmImages::default();
            pending
                .0
                .lock()
                .unwrap()
                .insert(path.to_string_lossy().into_owned(), image);
            let config = hai_core::UtmVmConfig {
                name: "Test VM".into(),
                image_path: path.to_string_lossy().into_owned(),
                cpu_cores: 2,
                memory_mb: 2048,
                disk_size_gb: 32,
                auto_start: false,
            };
            let backend = UtmCreationBackend {
                outcome,
                started: tokio::sync::Notify::new(),
            };
            if outcome == "cancelled" {
                let creation = run_utm_creation(&backend, &config, &pending);
                tokio::pin!(creation);
                tokio::select! {
                    _ = &mut creation => panic!("Creation must still be pending"),
                    _ = backend.started.notified() => {}
                }
                assert!(pending.take(&config.image_path).is_none());
                assert!(path.exists());
            } else {
                let result = run_utm_creation(&backend, &config, &pending).await;
                assert_eq!(
                    result.is_ok(),
                    matches!(outcome, "success" | "release-failure"),
                    "{outcome}"
                );
                if let Ok(id) = &result {
                    assert_eq!(id, "stable-utm-id");
                    assert_ne!(id, &config.name);
                }
                if matches!(outcome, "timeout" | "transport") {
                    let error = result.as_ref().unwrap_err();
                    assert!(error.contains(&format!(
                        "Source retained at {}.",
                        path.parent().unwrap().display()
                    )));
                    assert!(!error.contains(&config.image_path));
                    assert!(error.contains("manually"));
                }
                if outcome == "marker-failure" {
                    let error = result.unwrap_err();
                    assert!(error.contains("UTM was not contacted"));
                    assert!(error.contains(path.parent().unwrap().to_str().unwrap()));
                    assert!(error.contains("manually"));
                }
            }
            // This is the same removal used by the frontend's discard command.
            assert!(pending.take(&config.image_path).is_none());
            drop(pending);
            hai_core::download::prune_cached_images(cache.path()).unwrap();
            assert_eq!(
                path.exists(),
                !matches!(outcome, "success" | "rejected"),
                "{outcome}"
            );
        }
    }

    /// A request for `device_id` whose `expected_device` matches what the mock
    /// backend enumerates, so the flash-target guard lets it through.
    async fn request(device_id: &str, board: &str) -> FlashRequest {
        let device = BackendMock
            .list_devices()
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.id == device_id)
            .unwrap();
        FlashRequest {
            device_id: device.id,
            board: board.to_string(),
            verify: true,
            expected_device: ExpectedDevice {
                size: Some(device.size),
                model: device.model,
                vendor: device.vendor,
            },
        }
    }

    struct DigestFailureBackend {
        cache: tempfile::TempDir,
    }

    impl ReleaseSource for DigestFailureBackend {
        async fn get_device_manifest(&self) -> hai_core::Result<hai_core::DeviceManifest> {
            unreachable!()
        }

        async fn get_haos_release(&self, _: &str) -> hai_core::Result<HaosRelease> {
            unreachable!("must use the selected board's release")
        }

        async fn get_latest_haos_release_for_board(
            &self,
            board: &str,
        ) -> hai_core::Result<HaosRelease> {
            BackendMock.get_latest_haos_release_for_board(board).await
        }

        async fn download_image<P: ProgressCallback>(
            &self,
            image: &hai_core::HaosImage,
            _: &std::path::Path,
            _: &P,
        ) -> hai_core::Result<()> {
            assert!(image.download_url.contains(&image.board));
            Err(hai_core::Error::ChecksumMismatch {
                expected: "published digest".into(),
                actual: "tampered digest".into(),
            })
        }

        async fn extract_xz<P: ProgressCallback>(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
            _: &P,
        ) -> hai_core::Result<()> {
            panic!("unverified image reached extraction")
        }

        fn cache_dir(&self) -> hai_core::Result<std::path::PathBuf> {
            Ok(self.cache.path().to_path_buf())
        }
    }

    impl DeviceBackend for DigestFailureBackend {
        async fn list_devices(&self) -> hai_core::Result<Vec<BlockDevice>> {
            panic!("unverified image reached device preparation")
        }

        async fn write_image<P: ProgressCallback>(
            &self,
            _: &std::path::Path,
            _: &str,
            _: bool,
            _: &P,
        ) -> hai_core::Result<()> {
            panic!("unverified image reached the writer")
        }
    }

    #[tokio::test]
    async fn digest_failure_stops_flash_and_utm_before_extraction() {
        let backend = DigestFailureBackend {
            cache: tempfile::tempdir().unwrap(),
        };
        let error = run_flash(
            &backend,
            &request("mock-sd-card-32gb", "rpi5-64").await,
            &NoOpProgress,
        )
        .await
        .unwrap_err();
        assert!(error.contains("Checksum mismatch"));
        for board in ["generic-aarch64", "generic-x86-64"] {
            let error = run_utm_download(&backend, board, &NoOpProgress)
                .await
                .unwrap_err();
            assert!(error.contains("Checksum mismatch"));
        }
    }

    #[tokio::test]
    #[serial] // all share the mock cache directory
    async fn run_flash_completes_against_mock_backend() {
        let result = run_flash(
            &BackendMock,
            &request("mock-sd-card-32gb", "rpi5-64").await,
            &NoOpProgress,
        )
        .await
        .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    #[serial] // all share the mock cache directory
    async fn run_flash_rejects_unknown_board() {
        let err = run_flash(
            &BackendMock,
            &request("mock-sd-card-32gb", "no-such-board").await,
            &NoOpProgress,
        )
        .await
        .unwrap_err();
        assert!(err.contains("No image found"));
    }

    #[tokio::test]
    #[serial] // all share the mock cache directory
    async fn run_flash_rejects_non_removable_device() {
        let err = run_flash(
            &BackendMock,
            &request("mock-nvme-500gb", "rpi5-64").await,
            &NoOpProgress,
        )
        .await
        .unwrap_err();
        assert!(err.contains("not a removable drive"));
    }

    #[tokio::test]
    #[serial] // all share the mock cache directory
    async fn run_flash_rejects_swapped_device() {
        let mut request = request("mock-sd-card-32gb", "rpi5-64").await;
        request.expected_device.size = Some(64 * 1024 * 1024 * 1024);
        let err = run_flash(&BackendMock, &request, &NoOpProgress)
            .await
            .unwrap_err();
        assert!(err.contains("no longer the one you selected"));
    }

    #[tokio::test]
    #[serial] // all share the mock cache directory
    async fn run_utm_download_returns_extracted_image() {
        let image = run_utm_download(&BackendMock, "generic-aarch64", &NoOpProgress)
            .await
            .unwrap();
        let path = image.path();
        assert!(path.exists());
        drop(image);
        assert!(!path.exists());
    }
}

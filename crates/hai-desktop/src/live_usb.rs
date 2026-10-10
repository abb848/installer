//! Live USB commands: build a bootable stick or ISO for a mini PC with `hai-usb`.
//!
//! With the `mock` feature these commands only report fake progress. `hai-usb` always
//! uses the real backend, so calling it from a mock build would download and write for real.

use std::path::{Path, PathBuf};

use hai_core::{DeviceBackend, ExpectedDevice, FlashProgress, FlashStage, ProgressCallback};
use serde::Serialize;
use tauri::ipc::Channel;

use crate::backend::Backend;
use crate::command_error::CommandError;
use crate::commands::TauriProgressCallback;
use crate::flash_state::FlashState;

/// Lets testers point the app at a `hai-live` build kept anywhere, such as Downloads.
const HAI_LIVE_ENV: &str = "HAI_LIVE_PATH";
const HAI_LIVE_NAME: &str = "hai-live";

/// What the live USB flow needs before it can start.
#[derive(Debug, Serialize, PartialEq)]
pub struct LiveUsbStatus {
    /// The `hai-live` program for the stick was found.
    pub hai_live_found: bool,
    /// Writing a stick needs the app restarted as administrator (Windows only).
    pub needs_admin: bool,
}

/// Checks for `hai-live` and, on Windows, administrator rights.
#[tauri::command]
pub async fn live_usb_status() -> LiveUsbStatus {
    LiveUsbStatus {
        hai_live_found: cfg!(feature = "mock") || find_hai_live().is_some(),
        needs_admin: matches!(
            Backend.check_write_privileges(),
            Err(hai_core::Error::PermissionDenied(_))
        ),
    }
}

/// Downloads Alpine and HAOS (or reuses the cache), builds the stick image and writes it to
/// the removable drive `device_id` with read-back verification.
#[tauri::command]
pub async fn create_live_usb(
    device_id: String,
    expected_device: ExpectedDevice,
    progress_channel: Channel<FlashProgress>,
    state: tauri::State<'_, FlashState>,
) -> Result<(), CommandError> {
    state
        .run(async move {
            let callback = TauriProgressCallback::new(&progress_channel, "live_usb");
            let result = run_create_live_usb(&device_id, &expected_device, &callback).await;
            callback.operation.finish(result)
        })
        .await
}

/// Builds the ISO and saves it as `folder/file_name`, replacing an existing file only
/// with `overwrite`. Returns the saved path.
#[tauri::command]
pub async fn save_live_iso(
    folder: String,
    file_name: String,
    overwrite: bool,
    progress_channel: Channel<FlashProgress>,
    state: tauri::State<'_, FlashState>,
) -> Result<String, CommandError> {
    state
        .run(async move {
            let callback = TauriProgressCallback::new(&progress_channel, "live_iso");
            let result =
                run_save_live_iso(Path::new(&folder), &file_name, overwrite, &callback).await;
            callback
                .operation
                .finish(result)
                .map(|path| path.to_string_lossy().into_owned())
        })
        .await
}

/// Result of checking a save location before anything is downloaded.
#[derive(Debug, Serialize, PartialEq)]
pub struct IsoLocation {
    /// A file with this name is already there; saving replaces it.
    pub exists: bool,
}

/// Checks the name, that the folder exists and can be written, and whether the file exists.
#[tauri::command]
pub fn check_iso_location(folder: String, file_name: String) -> Result<IsoLocation, CommandError> {
    let folder = Path::new(&folder);
    let target = iso_target(folder, &file_name, true)?;
    if !cfg!(feature = "mock") {
        // Removed again at once; it only proves the folder can be written.
        tempfile::Builder::new()
            .prefix(".hai-live-")
            .tempdir_in(folder)?;
    }
    Ok(IsoLocation {
        exists: target.exists(),
    })
}

/// Opens the file manager with `path` selected.
#[tauri::command]
pub fn reveal_in_folder(app: tauri::AppHandle, path: String) -> Result<(), CommandError> {
    if cfg!(feature = "mock") {
        return Ok(());
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|_| CommandError::new("io", "Could not open the folder.", false))
}

/// Restarts the app as administrator. The flow starts again in the new window.
#[tauri::command]
pub fn relaunch_as_admin(app: tauri::AppHandle) -> Result<(), CommandError> {
    crate::elevation::relaunch_as_admin()?;
    app.exit(0);
    Ok(())
}

#[cfg(not(feature = "mock"))]
async fn run_create_live_usb<P: ProgressCallback>(
    device_id: &str,
    expected: &ExpectedDevice,
    callback: &P,
) -> Result<(), CommandError> {
    // Same order as the regular flash: no download when the write can't happen anyway.
    Backend
        .check_write_privileges()
        .map_err(CommandError::from)?;
    let hai_live = find_hai_live().ok_or_else(hai_live_missing)?;
    let contents = hai_usb::prepare(&hai_live, callback)
        .await
        .map_err(live_usb_error)?;

    report(
        callback,
        FlashStage::Extracting,
        "Preparing the USB stick...",
    );
    let temp = tempfile::Builder::new()
        .prefix("usb-image-")
        .tempdir_in(hai_usb::cache_dir().map_err(live_usb_error)?)?;
    let out_dir = temp.path().to_path_buf();
    let image = tokio::task::spawn_blocking(move || hai_usb::write_image(&contents, &out_dir))
        .await
        .map_err(|_| stopped())?
        .map_err(live_usb_error)?;

    hai_usb::write_to_usb(&image, device_id, expected, callback)
        .await
        .map_err(live_usb_error)?;
    drop(temp);
    report(callback, FlashStage::Complete, "USB stick ready");
    Ok(())
}

#[cfg(feature = "mock")]
async fn run_create_live_usb<P: ProgressCallback>(
    _device_id: &str,
    _expected: &ExpectedDevice,
    callback: &P,
) -> Result<(), CommandError> {
    for stage in [
        FlashStage::Downloading,
        FlashStage::Extracting,
        FlashStage::Writing,
        FlashStage::Verifying,
    ] {
        mock::simulate(callback, stage).await;
    }
    report(callback, FlashStage::Complete, "USB stick ready (mock)");
    Ok(())
}

async fn run_save_live_iso<P: ProgressCallback>(
    folder: &Path,
    file_name: &str,
    overwrite: bool,
    callback: &P,
) -> Result<PathBuf, CommandError> {
    let target = iso_target(folder, file_name, overwrite)?;
    #[cfg(feature = "mock")]
    {
        for stage in [FlashStage::Downloading, FlashStage::Extracting] {
            mock::simulate(callback, stage).await;
        }
        report(callback, FlashStage::Complete, "ISO saved (mock)");
        Ok(target)
    }
    #[cfg(not(feature = "mock"))]
    {
        let hai_live = find_hai_live().ok_or_else(hai_live_missing)?;
        // Created first, so an unwritable folder fails before anything is downloaded.
        let temp = tempfile::Builder::new()
            .prefix(".hai-live-")
            .tempdir_in(folder)?;
        let contents = hai_usb::prepare(&hai_live, callback)
            .await
            .map_err(live_usb_error)?;

        report(callback, FlashStage::Extracting, "Building the ISO...");
        let out_dir = temp.path().to_path_buf();
        let built = tokio::task::spawn_blocking(move || hai_usb::write_iso(&contents, &out_dir))
            .await
            .map_err(|_| stopped())?
            .map_err(live_usb_error)?;
        // hai-usb picks its own name; building in a temporary folder means an existing
        // file with that name next to the target is never overwritten.
        iso_target(folder, file_name, overwrite)?;
        if overwrite {
            match std::fs::remove_file(&target) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into())
                }
                _ => {}
            }
        }
        std::fs::rename(&built, &target)?;
        drop(temp);
        report(callback, FlashStage::Complete, "ISO saved");
        Ok(target)
    }
}

/// Where the ISO goes, refusing names that point elsewhere, and replacing an existing
/// file only with `overwrite`.
fn iso_target(folder: &Path, file_name: &str, overwrite: bool) -> Result<PathBuf, CommandError> {
    let name = file_name.trim();
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        return Err(CommandError::new(
            "invalid_file_name",
            "Enter a file name without folder separators.",
            false,
        ));
    }
    if !folder.is_dir() {
        return Err(CommandError::new(
            "folder_not_found",
            "The folder doesn't exist. Choose another folder.",
            false,
        ));
    }
    let target = folder.join(name);
    if target.is_dir() {
        return Err(CommandError::new(
            "folder_name_taken",
            "A folder with this name is already there. Choose another file name.",
            false,
        ));
    }
    if target.exists() && !overwrite {
        return Err(CommandError::new(
            "file_exists",
            "A file with this name already exists. Choose another name or folder.",
            false,
        ));
    }
    Ok(target)
}

/// The first place `hai-live` is found: the environment variable alone if it is set,
/// otherwise next to the app, then (developer builds) the repo's musl build folder.
fn find_hai_live() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(HAI_LIVE_ENV) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let next_to_app = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(HAI_LIVE_NAME)));
    let repo_build = cfg!(debug_assertions).then(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/x86_64-unknown-linux-musl/release")
            .join(HAI_LIVE_NAME)
    });
    [next_to_app, repo_build]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
}

#[cfg_attr(feature = "mock", allow(dead_code))]
fn hai_live_missing() -> CommandError {
    CommandError::new(
        "hai_live_missing",
        "The program for the USB stick (hai-live) wasn't found. Build it with \
         `cargo build --release -p hai-live --target x86_64-unknown-linux-musl`, \
         or set HAI_LIVE_PATH to its location, then restart the installer.",
        false,
    )
}

#[cfg_attr(feature = "mock", allow(dead_code))]
fn stopped() -> CommandError {
    CommandError::new(
        "internal",
        "Building the bootable media stopped unexpectedly.",
        false,
    )
}

/// Fixed messages only: `hai-usb` errors can contain paths and device names.
#[cfg_attr(feature = "mock", allow(dead_code))]
fn live_usb_error(error: hai_usb::Error) -> CommandError {
    use hai_usb::Error;
    match error {
        Error::Core(error) => error.into(),
        Error::Io(error) => error.into(),
        Error::NotLinuxBinary => CommandError::new(
            "hai_live_invalid",
            "The program for the USB stick (hai-live) is not the Linux build. Rebuild it \
             for x86_64-unknown-linux-musl, then restart the installer.",
            false,
        ),
        Error::NotRemovable(_) => CommandError::new(
            "invalid_config",
            "The selected drive is not removable, so it won't be written.",
            false,
        ),
        Error::DeviceChanged(_) => CommandError::new(
            "device_not_found",
            "The USB stick is no longer the one you selected. Select it again.",
            false,
        ),
        Error::MissingImage(_) | Error::MissingChecksum(_) | Error::InvalidVersion(_) => {
            CommandError::new(
                "release_unavailable",
                "The latest Home Assistant OS release can't be used right now. Try again later.",
                true,
            )
        }
        Error::DiskImage(_) | Error::Iso(_) => CommandError::new(
            "image_build_failed",
            "The bootable media could not be built. Check available storage, then try again.",
            true,
        ),
    }
}

fn report<P: ProgressCallback>(callback: &P, stage: FlashStage, message: &str) {
    let progress = if stage == FlashStage::Complete {
        100
    } else {
        0
    };
    callback.on_progress(FlashProgress {
        stage,
        progress,
        bytes_processed: 0,
        total_bytes: 0,
        message: message.to_string(),
    });
}

#[cfg(feature = "mock")]
mod mock {
    use super::*;

    const MOCK_BYTES: u64 = 500_000_000;

    /// A few seconds of progress for one stage, so the screens can be tried out.
    pub(super) async fn simulate<P: ProgressCallback>(callback: &P, stage: FlashStage) {
        let measured = stage != FlashStage::Extracting;
        for percent in (0..=100).step_by(10) {
            let bytes = MOCK_BYTES * percent / 100;
            callback.on_progress(FlashProgress {
                stage: stage.clone(),
                progress: percent as u8,
                bytes_processed: if measured { bytes } else { 0 },
                total_bytes: if measured { MOCK_BYTES } else { 0 },
                message: "mock".to_string(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_target_refuses_bad_names_and_replaces_files_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            iso_target(dir.path(), " stick.iso ", false).unwrap(),
            dir.path().join("stick.iso")
        );
        for name in ["", "  ", "..", "sub/stick.iso", "sub\\stick.iso"] {
            assert_eq!(
                iso_target(dir.path(), name, true).unwrap_err().code,
                "invalid_file_name",
                "{name:?}"
            );
        }
        assert_eq!(
            iso_target(&dir.path().join("missing"), "stick.iso", true)
                .unwrap_err()
                .code,
            "folder_not_found"
        );
        std::fs::write(dir.path().join("stick.iso"), b"old").unwrap();
        assert_eq!(
            iso_target(dir.path(), "stick.iso", false).unwrap_err().code,
            "file_exists"
        );
        assert!(iso_target(dir.path(), "stick.iso", true).is_ok());
        std::fs::create_dir(dir.path().join("taken.iso")).unwrap();
        assert_eq!(
            iso_target(dir.path(), "taken.iso", true).unwrap_err().code,
            "folder_name_taken"
        );
    }

    #[test]
    fn checking_a_location_reports_an_existing_file_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            check_iso_location(folder.clone(), "stick.iso".into()).unwrap(),
            IsoLocation { exists: false }
        );
        std::fs::write(dir.path().join("stick.iso"), b"old").unwrap();
        assert_eq!(
            check_iso_location(folder, "stick.iso".into()).unwrap(),
            IsoLocation { exists: true }
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn live_usb_errors_never_include_paths_or_device_names() {
        for error in [
            hai_usb::Error::NotRemovable(r"\\.\PhysicalDrive7".into()),
            hai_usb::Error::DeviceChanged("/dev/sdq".into()),
            hai_usb::Error::Iso(r"C:\Users\secret\x.iso".into()),
            hai_usb::Error::DiskImage("/home/secret/img".into()),
        ] {
            let message = live_usb_error(error).message;
            assert!(!message.contains("PhysicalDrive7"), "{message}");
            assert!(!message.contains("sdq"), "{message}");
            assert!(!message.contains("secret"), "{message}");
        }
        assert_eq!(
            live_usb_error(hai_usb::Error::NotLinuxBinary).code,
            "hai_live_invalid"
        );
    }

    #[test]
    #[serial_test::serial]
    fn hai_live_env_var_is_used_alone_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hai-live");
        std::env::set_var(HAI_LIVE_ENV, &path);
        assert_eq!(find_hai_live(), None);
        std::fs::write(&path, b"\x7fELF").unwrap();
        assert_eq!(find_hai_live(), Some(path));
        std::env::remove_var(HAI_LIVE_ENV);
    }
}

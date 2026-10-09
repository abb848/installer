//! Assembles the stick contents and turns them into an ISO, a disk image, or a written USB stick.

use std::path::{Path, PathBuf};

use hai_core::{ExpectedDevice, ProgressCallback};

use crate::stick_file::{Source, StickFile};
use crate::{apkovl, disk_image, downloads, iso, usb, Error, Result};

/// Everything that goes on the stick, ready to be written out.
#[derive(Debug, Clone)]
pub struct Contents {
    /// Unpacked Alpine ISO files.
    files: PathBuf,
    /// Our files at the root: apkovl, HAOS image and its checksum.
    extra: Vec<StickFile>,
    /// Alpine's ISO volume label, which our ISO must keep.
    label: String,
    /// The cache folder, used for the temporary image when writing a stick.
    cache: PathBuf,
    /// HAOS version on the stick.
    pub haos_version: String,
}

impl Contents {
    fn stem(&self) -> String {
        format!("hai-live-haos-{}", self.haos_version)
    }
}

/// Downloads (or reuses) Alpine and HAOS and assembles the stick contents.
/// `hai_live` is the static Linux build of the `hai-live` program.
pub async fn prepare<P: ProgressCallback>(hai_live: &Path, progress: &P) -> Result<Contents> {
    let hai_live = std::fs::read(hai_live)?;
    if !hai_live.starts_with(b"\x7fELF") {
        return Err(Error::NotLinuxBinary);
    }

    let cache = downloads::cache_dir()?;
    let alpine = downloads::fetch_alpine(&cache, progress).await?;
    let haos = downloads::fetch_haos(&cache, progress).await?;

    let files = unpack_alpine(&cache, &alpine)?;
    let label = iso::read_label(&alpine.path)?;
    let haos_name = haos
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Iso("HAOS file name is not valid UTF-8".into()))?
        .to_string();

    let extra = vec![
        StickFile {
            name: apkovl::APKOVL_NAME.into(),
            source: Source::Bytes(apkovl::build(&hai_live)?),
        },
        StickFile {
            name: format!("{haos_name}.sha256"),
            // Same format as `sha256sum`, so it can also be checked by hand on the stick.
            source: Source::Bytes(format!("{}  {haos_name}\n", haos.sha256).into_bytes()),
        },
        StickFile {
            name: haos_name,
            source: Source::File(haos.path.clone()),
        },
    ];

    Ok(Contents {
        files,
        extra,
        label,
        cache,
        haos_version: haos.version,
    })
}

/// Writes `<out_dir>/hai-live-haos-<version>.iso` and returns its path.
pub fn write_iso(contents: &Contents, out_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let path = out_dir.join(format!("{}.iso", contents.stem()));
    iso::build(&contents.files, &contents.extra, &contents.label, &path)?;
    Ok(path)
}

/// Writes `<out_dir>/hai-live-haos-<version>.img` (a raw disk image, handy for VMs) and returns its path.
pub fn write_image(contents: &Contents, out_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let path = out_dir.join(format!("{}.img", contents.stem()));
    disk_image::build(&path, &contents.files, &contents.extra)?;
    Ok(path)
}

/// Builds the stick image in a temporary folder, writes it to the USB drive `device_id`
/// with read-back verification, then deletes the temporary image.
pub async fn write_stick<P: ProgressCallback>(
    contents: &Contents,
    device_id: &str,
    expected: &ExpectedDevice,
    progress: &P,
) -> Result<()> {
    // Deleted when it goes out of scope, also on error.
    let temp = tempfile::Builder::new()
        .prefix("usb-image-")
        .tempdir_in(&contents.cache)?;
    let image = write_image(contents, temp.path())?;
    usb::write_to_usb(&image, device_id, expected, progress).await
}

/// Unpacks the Alpine ISO into the cache once per version and returns the folder.
fn unpack_alpine(cache: &Path, alpine: &downloads::Download) -> Result<PathBuf> {
    let dir = cache
        .join("work")
        .join(format!("alpine-{}", alpine.version));
    let done = dir.join(".unpacked");
    if done.exists() {
        return Ok(dir.join("files"));
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    iso::extract(&alpine.path, &dir.join("files"))?;
    std::fs::write(&done, alpine.sha256.as_bytes())?;
    Ok(dir.join("files"))
}

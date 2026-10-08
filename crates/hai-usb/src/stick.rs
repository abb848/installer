//! Assembles the stick contents and writes the USB disk image and the ISO.

use std::path::{Path, PathBuf};

use hai_core::ProgressCallback;

use crate::stick_file::{Source, StickFile};
use crate::{apkovl, disk_image, downloads, iso, Error, Result};

/// The files `build` produced.
#[derive(Debug, Clone)]
pub struct Outputs {
    /// Raw disk image for writing to a USB stick.
    pub disk_image: PathBuf,
    /// UEFI-bootable ISO for VMs and other flashing tools.
    pub iso: PathBuf,
    /// HAOS version on the stick.
    pub haos_version: String,
}

/// Downloads (or reuses) Alpine and HAOS, then writes `<out_dir>/hai-live-haos-<version>.img`
/// and `.iso`. `hai_live` is the static Linux build of the `hai-live` program.
pub async fn build<P: ProgressCallback>(
    hai_live: &Path,
    out_dir: &Path,
    progress: &P,
) -> Result<Outputs> {
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

    let extra = [
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

    std::fs::create_dir_all(out_dir)?;
    let stem = format!("hai-live-haos-{}", haos.version);
    let outputs = Outputs {
        disk_image: out_dir.join(format!("{stem}.img")),
        iso: out_dir.join(format!("{stem}.iso")),
        haos_version: haos.version,
    };
    disk_image::build(&outputs.disk_image, &files, &extra)?;
    iso::build(&files, &extra, &label, &outputs.iso)?;
    Ok(outputs)
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

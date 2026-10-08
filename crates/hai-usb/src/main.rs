//! Command line for testing `hai-usb` without the desktop app.
//!
//! Usage:
//!   hai-usb download
//!   hai-usb build <hai-live-linux-binary> <output-dir>
//!   hai-usb devices
//!   hai-usb write <image> <device-id>

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Mutex;

use hai_core::{BlockDevice, ExpectedDevice, FlashProgress, FlashStage, ProgressCallback};

const USAGE: &str = "usage:
  hai-usb download
  hai-usb build <hai-live-linux-binary> <output-dir>
  hai-usb devices
  hai-usb write <image> <device-id>";

/// Prints progress in whole 10% steps, starting again for each stage (download, write, verify).
#[derive(Default)]
struct PrintProgress {
    last: Mutex<Option<(FlashStage, u8)>>,
}

impl ProgressCallback for PrintProgress {
    fn on_progress(&self, progress: FlashProgress) {
        let step = progress.progress / 10 * 10;
        let current = Some((progress.stage.clone(), step));
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if *last != current {
            *last = current;
            println!("  {step:>3}%  {}", progress.message);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> hai_usb::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["download"] => download().await,
        ["build", hai_live, out_dir] => {
            let outputs = hai_usb::build(
                Path::new(hai_live),
                Path::new(out_dir),
                &PrintProgress::default(),
            )
            .await?;
            println!("Home Assistant OS {}", outputs.haos_version);
            println!("  USB image: {}", outputs.disk_image.display());
            println!("  ISO:       {}", outputs.iso.display());
            Ok(())
        }
        ["devices"] => devices().await,
        ["write", image, device_id] => write(Path::new(image), device_id).await,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn describe(drive: &BlockDevice) -> String {
    let model = [drive.vendor.as_deref(), drive.model.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{:<22} {:<30} {:>7.1} GB",
        drive.id,
        if model.is_empty() {
            &drive.name
        } else {
            &model
        },
        drive.size as f64 / 1e9
    )
}

async fn devices() -> hai_usb::Result<()> {
    let drives = hai_usb::list_usb_drives().await?;
    if drives.is_empty() {
        println!("No removable drives found.");
    }
    for drive in &drives {
        println!("{}", describe(drive));
    }
    Ok(())
}

async fn write(image: &Path, device_id: &str) -> hai_usb::Result<()> {
    if !image.is_file() {
        eprintln!("Image not found: {}", image.display());
        std::process::exit(1);
    }
    let drives = hai_usb::list_usb_drives().await?;
    let Some(drive) = drives.iter().find(|d| d.id == device_id) else {
        eprintln!("{device_id} is not a removable drive. Run `hai-usb devices` to list them.");
        std::process::exit(1);
    };
    println!("Writing {} to:\n\n  {}\n", image.display(), describe(drive));
    println!("EVERYTHING ON THIS DRIVE WILL BE ERASED.");
    print!("Type erase to continue: ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim() != "erase" {
        println!("Cancelled, nothing written.");
        return Ok(());
    }

    let expected = ExpectedDevice {
        size: Some(drive.size),
        model: drive.model.clone(),
        vendor: drive.vendor.clone(),
    };
    hai_usb::write_to_usb(image, device_id, &expected, &PrintProgress::default()).await?;
    println!("Done. The USB stick is ready.");
    Ok(())
}

async fn download() -> hai_usb::Result<()> {
    let cache = hai_usb::cache_dir()?;
    println!("cache: {}", cache.display());

    println!("Alpine {}:", hai_usb::ALPINE_VERSION);
    let alpine = hai_usb::fetch_alpine(&cache, &PrintProgress::default()).await?;
    println!("  {}\n  sha256 {}", alpine.path.display(), alpine.sha256);

    println!("Home Assistant OS ({}):", hai_usb::HAOS_BOARD);
    let haos = hai_usb::fetch_haos(&cache, &PrintProgress::default()).await?;
    println!(
        "  version {}\n  {}\n  sha256 {}",
        haos.version,
        haos.path.display(),
        haos.sha256
    );
    Ok(())
}

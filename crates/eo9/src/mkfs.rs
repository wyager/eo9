//! `eo9 mkfs.eofs` — create (if needed) and format a host image file with eofs, Eo9's
//! native filesystem (plan/14-eofs.md milestone 3).
//!
//! The command writes the image directly through `eofs-core` on a small file-backed
//! [`BlockDevice`]; at run time the *guest* never sees this code — it sees the raw
//! `eo9:disk` capability (the `--disk <image>` grant) and mounts the image by composing
//! the `fs.eofs` provider in front of its program (`fs.eofs $ …`).
//!
//! Safety posture: a file that already carries an eofs uberblock (even a damaged one) is
//! never reformatted without `--force` — surfacing the situation beats silent data loss.
//! A brand-new or all-blank file formats without ceremony, matching the provider's own
//! format-on-first-mount rule for blank devices (plan/14 D12).

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use eofs_core::format::{MAGIC, SLOT_OFFSETS, SLOT_SIZE};
use eofs_core::{BlockDevice, DeviceError, Eofs, FormatOptions};

use crate::cli::{ArgStream, Config, EXIT_SUCCESS, vlog};

/// Default image size when the file does not exist yet: 16 MiB, the same documented
/// default as the in-memory `disk.mem` device.
const DEFAULT_IMAGE_SIZE: u64 = 16 * 1024 * 1024;

/// `eo9 mkfs.eofs <image> [--size <bytes[K|M|G]>] [--force]`.
pub fn cmd_mkfs(cfg: &Config, stream: &mut ArgStream) -> Result<u8, String> {
    let mut image: Option<PathBuf> = None;
    let mut size: Option<u64> = None;
    let mut force = false;
    while let Some(token) = stream.next() {
        match token.as_str() {
            "--size" => {
                let value = stream
                    .next()
                    .ok_or_else(|| "option `--size` needs a value".to_string())?;
                size = Some(parse_size(&value)?);
            }
            "--force" => force = true,
            other if !other.starts_with('-') && image.is_none() => {
                image = Some(PathBuf::from(other));
            }
            other => return Err(format!("unknown argument `{other}` for `mkfs.eofs`")),
        }
    }
    let Some(image) = image else {
        return Err(
            "`mkfs.eofs` needs an image path: eo9 mkfs.eofs <image> [--size <bytes>] \
                    [--force]"
                .to_string(),
        );
    };

    let created = ensure_image_file(&image, size)?;
    let device = FileDevice::open(&image)
        .map_err(|err| format!("cannot open {}: {err}", image.display()))?;
    let device_size = device.size();

    if !created && carries_eofs_magic(&device)? && !force {
        return Err(format!(
            "{} already contains an eofs filesystem (or the remains of one); pass --force \
             to reformat it and lose its contents",
            image.display()
        ));
    }

    let formatted = Eofs::format(device, &FormatOptions::default())
        .map_err(|err| format!("cannot format {}: {err:?}", image.display()))?;
    vlog!(cfg, "formatted at txg {}", formatted.txg());
    println!(
        "formatted {}: {} bytes, eofs (block size {}, lz4 compression {})",
        image.display(),
        device_size,
        formatted.block_size(),
        if formatted.compression() { "on" } else { "off" },
    );
    Ok(EXIT_SUCCESS)
}

/// Make sure the image file exists. Returns `true` when this call created it. An existing
/// file keeps its size; `--size` on an existing file must match it (resizing an image in
/// place is not something a format command should do silently).
fn ensure_image_file(image: &Path, size: Option<u64>) -> Result<bool, String> {
    match std::fs::metadata(image) {
        Ok(meta) => {
            if !meta.is_file() {
                return Err(format!("{} is not a regular file", image.display()));
            }
            if let Some(size) = size
                && size != meta.len()
            {
                return Err(format!(
                    "{} already exists with a size of {} bytes; omit --size (or remove the \
                     file first to recreate it at {} bytes)",
                    image.display(),
                    meta.len(),
                    size
                ));
            }
            Ok(false)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let size = size.unwrap_or(DEFAULT_IMAGE_SIZE);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(image)
                .map_err(|err| format!("cannot create {}: {err}", image.display()))?;
            file.set_len(size)
                .map_err(|err| format!("cannot size {}: {err}", image.display()))?;
            Ok(true)
        }
        Err(err) => Err(format!("cannot inspect {}: {err}", image.display())),
    }
}

/// Whether either uberblock slot of the device starts with the eofs magic — the "this was
/// (or is) an eofs image" tell that makes a reformat require `--force`.
fn carries_eofs_magic(device: &FileDevice) -> Result<bool, String> {
    let mut slot = vec![0u8; MAGIC.len()];
    for offset in SLOT_OFFSETS {
        if offset + SLOT_SIZE > device.size() {
            continue;
        }
        device
            .read_at(offset, &mut slot)
            .map_err(|err| format!("cannot read the image header: {err}"))?;
        if slot == MAGIC {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `--size` values: plain bytes, or binary-suffixed `K`/`M`/`G`.
fn parse_size(value: &str) -> Result<u64, String> {
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b'K' | b'k') => (&value[..value.len() - 1], 1024u64),
        Some(b'M' | b'm') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'G' | b'g') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let count: u64 = digits
        .parse()
        .map_err(|err| format!("invalid --size value {value:?}: {err}"))?;
    count
        .checked_mul(multiplier)
        .ok_or_else(|| format!("--size value {value:?} overflows"))
}

/// A [`BlockDevice`] over a host file, used only by this command to format images. The
/// run-time path never touches it: programs reach the image through the `eo9:disk`
/// capability and the unix disk provider.
pub(crate) struct FileDevice {
    file: std::fs::File,
    size: u64,
}

impl FileDevice {
    pub(crate) fn open(path: &Path) -> io::Result<FileDevice> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileDevice { file, size })
    }
}

impl BlockDevice for FileDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        check_range(self.size, offset, buf.len())?;
        self.file
            .read_exact_at(buf, offset)
            .map_err(|_| DeviceError::Io)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        check_range(self.size, offset, data.len())?;
        self.file
            .write_all_at(data, offset)
            .map_err(|_| DeviceError::Io)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        self.file.sync_data().map_err(|_| DeviceError::Io)
    }
}

fn check_range(size: u64, offset: u64, len: usize) -> Result<(), DeviceError> {
    let end = offset
        .checked_add(len as u64)
        .ok_or(DeviceError::OutOfRange)?;
    if end > size {
        return Err(DeviceError::OutOfRange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_values_accept_binary_suffixes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_size("16M").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn file_device_round_trips_and_bounds_accesses() {
        let dir = std::env::temp_dir().join(format!("eo9-mkfs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev.img");
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            file.set_len(8192).unwrap();
        }
        let mut device = FileDevice::open(&path).unwrap();
        assert_eq!(device.size(), 8192);
        device.write_at(100, b"persist").unwrap();
        device.flush().unwrap();
        let mut back = [0u8; 7];
        device.read_at(100, &mut back).unwrap();
        assert_eq!(&back, b"persist");
        assert_eq!(
            device.write_at(8190, &[0; 8]).unwrap_err(),
            DeviceError::OutOfRange
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

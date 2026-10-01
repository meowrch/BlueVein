use crate::config::BlueVeinConfig;
use crate::log;
use fat32_raw::Fat32Volume;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub enum EfiError {
    NotFound,
    ReadError(String),
    WriteError(String),
    ParseError(String),
}

impl fmt::Display for EfiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EfiError::NotFound => write!(f, "Configuration file not found on EFI partition"),
            EfiError::ReadError(msg) => write!(f, "Failed to read from EFI: {}", msg),
            EfiError::WriteError(msg) => write!(f, "Failed to write to EFI: {}", msg),
            EfiError::ParseError(msg) => write!(f, "Failed to parse config: {}", msg),
        }
    }
}

impl Error for EfiError {}

const CONFIG_FILENAME: &str = "bluevein.json";

// Common EFI mount points
#[cfg(target_os = "linux")]
#[allow(dead_code)]
const EFI_MOUNT_POINTS: &[&str] = &["/boot/efi", "/efi", "/boot"];

/// EFI context with device path
pub struct EfiContext {
    pub device: String,
}

impl EfiContext {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
        }
    }

    pub fn from_env() -> Self {
        env::var("BLUEVEIN_EFI_DEVICE")
            .ok()
            .map(Self::new)
            .unwrap_or_default()
    }

    pub fn display_name(&self) -> &str {
        if self.device.is_empty() {
            "auto-detected"
        } else {
            &self.device
        }
    }

    pub fn validate(&self) -> Result<(), EfiError> {
        if self.device.is_empty() {
            return Ok(());
        }

        Fat32Volume::open_esp(Some(&self.device))
            .map_err(|e| EfiError::ReadError(format!("Failed to open ESP partition: {}", e)))?
            .ok_or_else(|| EfiError::ReadError("ESP partition not found".to_string()))?;

        Ok(())
    }
}

impl Default for EfiContext {
    fn default() -> Self {
        Self::new("")
    }
}

/// Find mounted EFI partition path
fn find_mounted_efi() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        for mount_point in EFI_MOUNT_POINTS {
            let path = Path::new(mount_point);
            if !(path.exists() && path.is_dir()) { continue; }

            // findmnt for robustness, can check filesystem type
            let check_mount = Command::new("findmnt")
                .arg("-n")
                .arg("-o")
                .arg("FSTYPE")
                .arg(mount_point)
                .output();

            if let Ok(output) = check_mount {
                let fstype = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let efi_dir = path.join("EFI");

                // Check if it's actually mounted and looks like EFI
                if fstype == "vfat" && efi_dir.exists() && efi_dir.is_dir() {
                    return Some(mount_point.to_string());
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn mounted_efi_for_device(device: Option<&str>) -> Result<Option<std::path::PathBuf>, String> {
    let Some(device) = device.filter(|value| !value.is_empty()) else {
        return Ok(find_mounted_efi().map(Into::into));
    };
    let requested = fs::canonicalize(device)
        .map_err(|error| format!("Cannot resolve EFI device {device}: {error}"))?;
    let mounts = fs::read_to_string("/proc/self/mounts")
        .map_err(|error| format!("Cannot inspect mounted filesystems: {error}"))?;
    for line in mounts.lines() {
        let mut columns = line.split_whitespace();
        let (Some(source), Some(target), Some(fs_type)) =
            (columns.next(), columns.next(), columns.next()) else { continue };
        if fs::canonicalize(source).ok().as_deref() != Some(requested.as_path()) {
            continue;
        }
        let mount = Path::new(target);
        if fs_type != "vfat" || !mount.join("EFI").is_dir() {
            return Err(format!("EFI device is mounted at {target} without a usable EFI directory"));
        }
        return Ok(Some(mount.to_path_buf()));
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
fn mounted_efi_for_device(_device: Option<&str>) -> Result<Option<std::path::PathBuf>, String> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn write_mounted_config(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().ok_or_else(|| std::io::Error::other("No EFI directory"))?;
    for attempt in 0..100 {
        let temporary = directory.join(format!(".bluevein-{}-{attempt}.tmp", std::process::id()));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary);
        let mut file = match file {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(data)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            let directory_handle = fs::File::open(directory)?;
            directory_handle.sync_all()?;
            // FAT metadata may still be delayed after fsync(file) and fsync(dir).
            // Windows and the raw reader must see the same cluster chain now.
            if unsafe { libc::syncfs(directory_handle.as_raw_fd()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "No free EFI temporary name"))
}

fn find_json_end(data: &[u8]) -> usize {
    let (mut depth, mut in_str, mut esc) = (0u32, false, false);
    for (i, &b) in data.iter().enumerate() {
        if esc {
            esc = false;
            continue;
        }
        if in_str {
            match b {
                b'"' => in_str = false,
                b'\\' => esc = true,
                _ => {}
            }
        } else {
            match b {
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                b'"' => in_str = true,
                _ => {}
            }
        }
    }
    data.len()
}

/// Read BlueVein configuration from EFI partition using default device
#[allow(dead_code)]
pub fn read_config() -> Result<BlueVeinConfig, EfiError> {
    read_config_with_device(None)
}

/// Read BlueVein configuration from EFI partition
///
/// # Arguments
/// * `device` - If Some, use direct disk access with specified device
///              If None, try mounted EFI first, then fallback to default device
pub fn read_config_with_device(device: Option<&str>) -> Result<BlueVeinConfig, EfiError> {
    // Reading a mounted FAT volume through its raw block device can see stale
    // clusters after the kernel has updated its cached FAT and directory.
    if let Some(mount_point) = mounted_efi_for_device(device).map_err(EfiError::ReadError)? {
        let config_path = mount_point.join(CONFIG_FILENAME);
        let json_str = match fs::read_to_string(&config_path) {
            Ok(json_str) => json_str,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(EfiError::NotFound),
            Err(error) => return Err(EfiError::ReadError(error.to_string())),
        };
        return BlueVeinConfig::from_json(&json_str)
            .map_err(|error| EfiError::ParseError(error.to_string()));
    }

    // Use specified device or empty (will fail if not mounted and no device specified)
    let device_path = device.unwrap_or("");

    // Fallback to direct disk access via fat32-raw
    let mut volume = Fat32Volume::open_esp(if device_path.is_empty() {
        None
    } else {
        Some(device_path)
    })
    .map_err(|e| EfiError::ReadError(format!("Failed to open ESP partition: {}", e)))?
    .ok_or_else(|| EfiError::ReadError("ESP partition not found".to_string()))?;

    match volume.read_file(CONFIG_FILENAME) {
        Ok(Some(mut data)) => {
            let end = find_json_end(&data);
            if end < data.len() {
                log!("[BlueVein] Truncated {} trailing bytes from config", data.len() - end);
                data.truncate(end);
            }
            let json_str = String::from_utf8(data).map_err(|e| {
                EfiError::ParseError(format!("Invalid UTF-8 in config file: {}", e))
            })?;

            BlueVeinConfig::from_json(&json_str).map_err(|e| EfiError::ParseError(e.to_string()))
        }
        Ok(None) => Err(EfiError::NotFound),
        Err(e) => Err(EfiError::ReadError(format!(
            "Failed to read {}: {}",
            CONFIG_FILENAME, e
        ))),
    }
}

/// Write BlueVein configuration to EFI partition using default device
#[allow(dead_code)]
pub fn write_config(config: &BlueVeinConfig) -> Result<(), EfiError> {
    write_config_with_device(config, None)
}

/// Write BlueVein configuration to EFI partition
///
/// # Arguments
/// * `device` - If Some, use direct disk access with specified device
///              If None, try mounted filesystem first, then fallback to default device
pub fn write_config_with_device(
    config: &BlueVeinConfig,
    device: Option<&str>,
) -> Result<(), EfiError> {
    // Serialize config to JSON
    let json = config
        .to_json()
        .map_err(|e| EfiError::WriteError(format!("Failed to serialize config: {}", e)))?;

    // An explicit device path must not force raw writes while Linux has the
    // same filesystem mounted. Use a synced replacement through the mount.
    if let Some(mount_point) = mounted_efi_for_device(device).map_err(EfiError::WriteError)? {
        let config_path = mount_point.join(CONFIG_FILENAME);
        #[cfg(target_os = "linux")]
        write_mounted_config(&config_path, json.as_bytes())
            .map_err(|error| EfiError::WriteError(error.to_string()))?;
        log!("[BlueVein] Wrote config via mounted filesystem: {}", config_path.display());
        return Ok(());
    }

    // Use specified device or empty (will fail if not mounted and no device specified)
    let device_path = device.unwrap_or("");

    // Fallback to direct disk access via fat32-raw
    log!("[BlueVein] Using direct disk access via fat32-raw");

    let mut volume = Fat32Volume::open_esp(if device_path.is_empty() {
        None
    } else {
        Some(device_path)
    })
    .map_err(|e| EfiError::WriteError(format!("Failed to open ESP partition: {}", e)))?
    .ok_or_else(|| EfiError::WriteError("ESP partition not found".to_string()))?;

    // Check if file exists
    match volume.read_file(CONFIG_FILENAME) {
        Ok(Some(_)) => {
            // File exists, overwrite it
            volume
                .write_file(CONFIG_FILENAME, json.as_bytes())
                .map_err(|e| {
                    EfiError::WriteError(format!("Failed to write {}: {}", CONFIG_FILENAME, e))
                })?;
        }
        Ok(None) | Err(_) => {
            // File doesn't exist, create it
            volume.create_file_lfn(CONFIG_FILENAME).map_err(|e| {
                EfiError::WriteError(format!("Failed to create {}: {}", CONFIG_FILENAME, e))
            })?;

            volume
                .write_file(CONFIG_FILENAME, json.as_bytes())
                .map_err(|e| {
                    EfiError::WriteError(format!("Failed to write {}: {}", CONFIG_FILENAME, e))
                })?;
        }
    }

    // Call sync to flush buffers
    #[cfg(target_os = "linux")]
    {
        unsafe {
            libc::sync();
        }
    }

    Ok(())
}

/// Injectable shared store: tests exercise synchronization without touching disks.
pub trait ConfigStore: Send {
    fn read(&self) -> Result<BlueVeinConfig, EfiError>;
    fn write(&mut self, config: &BlueVeinConfig) -> Result<(), EfiError>;
    fn display_name(&self) -> &str;
}

impl ConfigStore for EfiContext {
    fn read(&self) -> Result<BlueVeinConfig, EfiError> {
        read_config_with_device(Some(&self.device))
    }
    fn write(&mut self, config: &BlueVeinConfig) -> Result<(), EfiError> {
        write_config_with_device(config, Some(&self.device))
    }
    fn display_name(&self) -> &str { EfiContext::display_name(self) }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn mounted_config_is_published_with_private_permissions() {
        let root = std::env::temp_dir().join(format!(
            "bluevein-mounted-config-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(CONFIG_FILENAME);

        write_mounted_config(&path, b"private test data").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"private test data");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(root).unwrap();
    }
}

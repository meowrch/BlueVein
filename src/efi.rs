use crate::config::BlueVeinConfig;
use crate::log;
use fat32_raw::Fat32Volume;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
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
    // If device is explicitly specified, skip mounted filesystem check
    if device.is_none() {
        // Try mounted filesystem first (faster and no cache issues)
        if let Some(mount_point) = find_mounted_efi() {
            let config_path = Path::new(&mount_point).join(CONFIG_FILENAME);

            if config_path.exists() {
                match fs::read_to_string(&config_path) {
                    Ok(json_str) => {
                        return BlueVeinConfig::from_json(&json_str)
                            .map_err(|e| EfiError::ParseError(e.to_string()));
                    }
                    Err(e) => {
                        log!("[BlueVein] Warning: Failed to read from mounted EFI ({}), trying direct access", e);
                        // Fall through to fat32-raw
                    }
                }
            } else {
                // File doesn't exist
                return Err(EfiError::NotFound);
            }
        }
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

    // If device is not explicitly specified, try mounted filesystem first
    if device.is_none() {
        if let Some(mount_point) = find_mounted_efi() {
            let config_path = Path::new(&mount_point).join(CONFIG_FILENAME);

            match fs::write(&config_path, &json) {
                Ok(_) => {
                    // Sync to ensure data is flushed to disk
                    #[cfg(target_os = "linux")]
                    {
                        unsafe {
                            libc::sync();
                        }
                    }

                    log!(
                        "[BlueVein] Wrote config via mounted filesystem: {}",
                        config_path.display()
                    );
                    return Ok(());
                }
                Err(e) => {
                    log!(
                        "[BlueVein] Warning: Failed to write to mounted EFI ({}), trying direct access",
                        e
                    );
                    // Fall through to fat32-raw
                }
            }
        }
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

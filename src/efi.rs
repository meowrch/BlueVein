use crate::config::BlueVeinConfig;
use crate::log;
use fat32_raw::Fat32Volume;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

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
const EFI_MOUNT_POINTS: &[&str] = &["/boot/efi", "/efi", "/boot"];

/// Find mounted EFI partition path
fn find_mounted_efi() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        for mount_point in EFI_MOUNT_POINTS {
            let path = Path::new(mount_point);
            if path.exists() && path.is_dir() {
                // Check if it's actually mounted and looks like EFI
                let efi_dir = path.join("EFI");
                if efi_dir.exists() {
                    return Some(mount_point.to_string());
                }
            }
        }
    }
    None
}

/// Read BlueVein configuration from EFI partition
pub fn read_config() -> Result<BlueVeinConfig, EfiError> {
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

    // Fallback to direct disk access via fat32-raw
    let mut volume = Fat32Volume::open_esp(None::<&str>)
        .map_err(|e| EfiError::ReadError(format!("Failed to open ESP partition: {}", e)))?
        .ok_or_else(|| EfiError::ReadError("ESP partition not found".to_string()))?;

    match volume.read_file(CONFIG_FILENAME) {
        Ok(Some(data)) => {
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

/// Write BlueVein configuration to EFI partition
pub fn write_config(config: &BlueVeinConfig) -> Result<(), EfiError> {
    // Serialize config to JSON
    let json = config
        .to_json()
        .map_err(|e| EfiError::WriteError(format!("Failed to serialize config: {}", e)))?;

    // Try mounted filesystem first (preferred method - no cache issues)
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

    // Fallback to direct disk access via fat32-raw
    log!("[BlueVein] Using direct disk access via fat32-raw");

    let mut volume = Fat32Volume::open_esp(None::<&str>)
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

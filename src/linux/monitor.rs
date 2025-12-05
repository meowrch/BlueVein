use crate::log;
use crate::sync::SyncManager;
use inotify::{Inotify, WatchMask};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

const BLUETOOTH_LIB_PATH: &str = "/var/lib/bluetooth";

pub async fn monitor_bluetooth_changes(
    mut sync_manager: SyncManager,
) -> Result<(), Box<dyn Error>> {
    let mut inotify = Inotify::init()?;
    let mut watches = HashMap::new();

    // Watch main bluetooth directory
    let main_watch = inotify.watches().add(
        BLUETOOTH_LIB_PATH,
        WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_TO | WatchMask::MOVED_FROM,
    )?;
    watches.insert(main_watch.clone(), PathBuf::from(BLUETOOTH_LIB_PATH));

    // Add watches for existing adapter directories and their device subdirectories
    if let Ok(entries) = fs::read_dir(BLUETOOTH_LIB_PATH) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();

                // Check if it looks like an adapter (MAC address)
                if name.contains(':') && name.len() == 17 {
                    // Watch adapter directory
                    if let Ok(watch) = inotify.watches().add(
                        &path,
                        WatchMask::CREATE
                            | WatchMask::DELETE
                            | WatchMask::MODIFY
                            | WatchMask::MOVED_TO
                            | WatchMask::MOVED_FROM,
                    ) {
                        watches.insert(watch, path.clone());
                        log!("[BlueVein] Watching adapter: {}", name);
                    }

                    // Watch device directories inside adapter
                    add_device_watches(&mut inotify, &mut watches, &path);
                }
            }
        }
    }

    log!(
        "[BlueVein] Monitoring {} for Bluetooth changes...",
        BLUETOOTH_LIB_PATH
    );

    let mut buffer = [0; 4096];
    loop {
        let events = inotify.read_events_blocking(&mut buffer)?;

        for event in events {
            if let Some(name) = event.name {
                let name_str = name.to_string_lossy().to_string();

                // Get the base path for this watch (clone to avoid borrow issues)
                let base_path = watches.get(&event.wd).cloned();

                if let Some(base_path) = base_path {
                    let full_path = base_path.join(&name_str);

                    // Check if this is an adapter directory in main path
                    if base_path.to_str() == Some(BLUETOOTH_LIB_PATH) {
                        if name_str.contains(':') && name_str.len() == 17 {
                            if event.mask.contains(inotify::EventMask::CREATE)
                                || event.mask.contains(inotify::EventMask::MOVED_TO)
                            {
                                // New adapter detected, add watch
                                if let Ok(watch) = inotify.watches().add(
                                    &full_path,
                                    WatchMask::CREATE
                                        | WatchMask::DELETE
                                        | WatchMask::MODIFY
                                        | WatchMask::MOVED_TO
                                        | WatchMask::MOVED_FROM,
                                ) {
                                    watches.insert(watch, full_path.clone());
                                    log!("[BlueVein] New adapter detected: {}", name_str);

                                    // Watch devices in new adapter
                                    add_device_watches(&mut inotify, &mut watches, &full_path);
                                }
                            }
                        }
                    } else if name_str == "info" {
                        // This is an info file change - extract device and adapter MAC
                        if let Some(device_mac) = base_path.file_name().and_then(|n| n.to_str()) {
                            if let Some(adapter_path) = base_path.parent() {
                                if let Some(adapter_mac) =
                                    adapter_path.file_name().and_then(|n| n.to_str())
                                {
                                    if event.mask.contains(inotify::EventMask::MODIFY)
                                        || event.mask.contains(inotify::EventMask::CLOSE_WRITE)
                                    {
                                        log!("[BlueVein] Info file updated for device {} on adapter {}", device_mac, adapter_mac);

                                        // Check if pairing keys (Classic or LE) exist now
                                        if has_pairing_keys(&full_path) {
                                            log!("[BlueVein] Pairing keys detected, syncing...");
                                            if let Err(e) = sync_manager
                                                .handle_device_change(adapter_mac, device_mac)
                                            {
                                                log!("[BlueVein] Failed to sync device: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // This is a device change within an adapter directory
                        if name_str.contains(':') && name_str.len() == 17 {
                            let adapter_mac =
                                base_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                            if event.mask.contains(inotify::EventMask::DELETE)
                                || event.mask.contains(inotify::EventMask::MOVED_FROM)
                            {
                                // Device removed
                                log!(
                                    "[BlueVein] Device removal detected: {} on adapter {}",
                                    name_str,
                                    adapter_mac
                                );
                                if let Err(e) =
                                    sync_manager.handle_device_removal(adapter_mac, &name_str)
                                {
                                    log!("[BlueVein] Failed to handle device removal: {}", e);
                                }
                            } else if event.mask.contains(inotify::EventMask::CREATE)
                                || event.mask.contains(inotify::EventMask::MOVED_TO)
                            {
                                // New device directory created - add watch for info file
                                log!(
                                    "[BlueVein] New device directory detected: {} on adapter {}",
                                    name_str,
                                    adapter_mac
                                );
                                add_device_watches(&mut inotify, &mut watches, &base_path);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Add watches for device directories and their info files
fn add_device_watches(
    inotify: &mut Inotify,
    watches: &mut HashMap<inotify::WatchDescriptor, PathBuf>,
    adapter_path: &PathBuf,
) {
    if let Ok(entries) = fs::read_dir(adapter_path) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let device_path = entry.path();
                let device_name = entry.file_name().to_string_lossy().to_string();

                // Check if it looks like a device (MAC address)
                if device_name.contains(':') && device_name.len() == 17 {
                    // Watch device directory for info file changes
                    if let Ok(watch) = inotify.watches().add(
                        &device_path,
                        WatchMask::MODIFY | WatchMask::CREATE | WatchMask::CLOSE_WRITE,
                    ) {
                        watches.insert(watch, device_path);
                    }
                }
            }
        }
    }
}

/// Check if info file contains pairing keys (Classic LinkKey or LE keys)
/// 
/// This function detects both:
/// - Classic Bluetooth: [LinkKey] section with Key=
/// - Bluetooth LE: [LongTermKey], [PeripheralLongTermKey], or [IdentityResolvingKey]
/// 
/// Returns true if ANY pairing key is found, indicating the device has been paired.
fn has_pairing_keys(info_path: &PathBuf) -> bool {
    if let Ok(content) = fs::read_to_string(info_path) {
        let lines: Vec<&str> = content.lines().collect();
        let mut current_section = String::new();

        for line in lines {
            let trimmed = line.trim();
            
            // Track current section
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                current_section = trimmed.to_string();
            } else if trimmed.starts_with("Key=") {
                // Check if we're in a pairing key section
                match current_section.as_str() {
                    "[LinkKey]" | "[LongTermKey]" | "[PeripheralLongTermKey]" | "[IdentityResolvingKey]" | "[SlaveLongTermKey]" => {
                        let key_value = trimmed.strip_prefix("Key=").unwrap_or("");
                        // Validate key is not empty and has valid hex format (32 chars = 128-bit)
                        if key_value.len() == 32 && key_value.chars().all(|c| c.is_ascii_hexdigit()) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

use crate::bluetooth::windows_format_to_mac;
use crate::log;
use crate::sync::SyncManager;
use std::collections::HashMap;
use std::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Registry::{
    RegNotifyChangeKeyValue, HKEY, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_NOTIFY, KEY_READ};
use winreg::RegKey;

const BLUETOOTH_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Keys";

#[derive(Debug, Clone)]
struct BluetoothState {
    adapters: HashMap<String, AdapterInfo>,
}

#[derive(Debug, Clone)]
struct AdapterInfo {
    devices: HashMap<String, Vec<u8>>,
}

impl BluetoothState {
    fn new() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }
}

pub fn monitor_bluetooth_changes(
    mut sync_manager: SyncManager,
    running: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
    log!("[BlueVein] Starting Windows registry monitoring...");

    // Read initial state
    let mut previous_state = read_bluetooth_state()?;
    log!(
        "[BlueVein] Initial state: {} adapters",
        previous_state.adapters.len()
    );

    while running.load(Ordering::Relaxed) {
        match wait_for_registry_change(&running) {
            Ok(true) => {
                // Change detected
                log!("[BlueVein] Registry change detected");

                // Small delay to allow registry to settle
                thread::sleep(Duration::from_millis(100));

                match read_bluetooth_state() {
                    Ok(new_state) => {
                        detect_and_handle_changes(&mut sync_manager, &previous_state, &new_state);
                        previous_state = new_state;
                    }
                    Err(e) => log!("[BlueVein] Error reading new state: {}", e),
                }
            }
            Ok(false) => {
                // Service stopping
                break;
            }
            Err(e) => {
                log!("[BlueVein] Monitoring error: {}", e);
                thread::sleep(Duration::from_secs(5));
            }
        }
    }

    log!("[BlueVein] Monitoring stopped");
    Ok(())
}

fn wait_for_registry_change(running: &Arc<AtomicBool>) -> Result<bool, Box<dyn Error>> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let bt_keys = hklm
        .open_subkey_with_flags(BLUETOOTH_REG_PATH, KEY_READ | KEY_NOTIFY)
        .map_err(|e| format!("Failed to open Bluetooth registry key: {}", e))?;

    // Get native HKEY handle
    let hkey = HKEY(bt_keys.raw_handle() as *mut core::ffi::c_void);

    // Wait for registry changes (blocking call)
    unsafe {
        let result = RegNotifyChangeKeyValue(
            hkey,
            true, // watch subtree
            REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
            HANDLE::default(),
            false, // synchronous
        );

        if result.is_ok() {
            // Change detected, check if we should continue
            Ok(running.load(Ordering::Relaxed))
        } else {
            Err(format!("RegNotifyChangeKeyValue failed: {:?}", result).into())
        }
    }
}

fn read_bluetooth_state() -> Result<BluetoothState, Box<dyn Error>> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let bt_keys = hklm
        .open_subkey_with_flags(BLUETOOTH_REG_PATH, KEY_READ)
        .map_err(|e| format!("Failed to open Bluetooth registry key: {}", e))?;

    let mut state = BluetoothState::new();

    for adapter_result in bt_keys.enum_keys() {
        if let Ok(adapter_key_name) = adapter_result {
            if let Ok(adapter_subkey) = bt_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ)
            {
                let mut devices = HashMap::new();

                for value_result in adapter_subkey.enum_values() {
                    if let Ok((device_mac, value_data)) = value_result {
                        // Skip special keys like "CentralIRK"
                        if device_mac.len() == 12
                            && device_mac.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            devices.insert(device_mac, value_data.bytes);
                        }
                    }
                }

                state
                    .adapters
                    .insert(adapter_key_name, AdapterInfo { devices });
            }
        }
    }

    Ok(state)
}

fn detect_and_handle_changes(
    sync_manager: &mut SyncManager,
    old_state: &BluetoothState,
    new_state: &BluetoothState,
) {
    // Check for new adapters
    for (adapter_mac, adapter_info) in &new_state.adapters {
        if !old_state.adapters.contains_key(adapter_mac) {
            log!("[BlueVein] New adapter detected: {}", adapter_mac);

            // Sync all devices from this new adapter
            for device_mac in adapter_info.devices.keys() {
                let normalized_adapter = windows_format_to_mac(adapter_mac);
                let normalized_device = windows_format_to_mac(device_mac);

                if let Err(e) =
                    sync_manager.handle_device_change(&normalized_adapter, &normalized_device)
                {
                    log!("[BlueVein] Failed to sync new adapter device: {}", e);
                }
            }
        }
    }

    // Check for removed adapters
    for adapter_mac in old_state.adapters.keys() {
        if !new_state.adapters.contains_key(adapter_mac) {
            log!("[BlueVein] Adapter removed: {}", adapter_mac);
        }
    }

    // Check for device changes within each adapter
    for (adapter_mac, new_adapter_info) in &new_state.adapters {
        if let Some(old_adapter_info) = old_state.adapters.get(adapter_mac) {
            // Check for new or modified devices
            for (device_mac, device_key) in &new_adapter_info.devices {
                let normalized_adapter = windows_format_to_mac(adapter_mac);
                let normalized_device = windows_format_to_mac(device_mac);

                match old_adapter_info.devices.get(device_mac) {
                    None => {
                        // New device
                        log!(
                            "[BlueVein] New device paired: {} on adapter {}",
                            device_mac,
                            adapter_mac
                        );

                        if let Err(e) = sync_manager
                            .handle_device_change(&normalized_adapter, &normalized_device)
                        {
                            log!("[BlueVein] Failed to sync new device: {}", e);
                        }
                    }
                    Some(old_key) if old_key != device_key => {
                        // Device key changed
                        log!(
                            "[BlueVein] Device key changed: {} on adapter {}",
                            device_mac,
                            adapter_mac
                        );

                        if let Err(e) = sync_manager
                            .handle_device_change(&normalized_adapter, &normalized_device)
                        {
                            log!("[BlueVein] Failed to sync device change: {}", e);
                        }
                    }
                    _ => {}
                }
            }

            // Check for removed devices
            for device_mac in old_adapter_info.devices.keys() {
                if !new_adapter_info.devices.contains_key(device_mac) {
                    let normalized_adapter = windows_format_to_mac(adapter_mac);
                    let normalized_device = windows_format_to_mac(device_mac);

                    log!(
                        "[BlueVein] Device removed: {} from adapter {}",
                        device_mac,
                        adapter_mac
                    );

                    if let Err(e) =
                        sync_manager.handle_device_removal(&normalized_adapter, &normalized_device)
                    {
                        log!("[BlueVein] Failed to handle device removal: {}", e);
                    }
                }
            }
        }
    }
}

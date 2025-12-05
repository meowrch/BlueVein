use crate::bluetooth::{BluetoothDevice, BluetoothManager, CsrkKey};
use crate::config::BlueVeinConfig;
use crate::efi;
use crate::log;
use std::collections::HashMap;
use std::error::Error;

/// Synchronization manager
pub struct SyncManager {
    bt_manager: Box<dyn BluetoothManager>,
}

impl SyncManager {
    /// Create a new sync manager
    pub fn new(bt_manager: Box<dyn BluetoothManager>) -> Self {
        Self { bt_manager }
    }

    /// Compare two devices to see if their keys differ
    fn devices_differ(dev1: &BluetoothDevice, dev2: &BluetoothDevice) -> bool {
        if dev1.classic != dev2.classic {
            return true;
        }
        if dev1.le != dev2.le {
            return true;
        }
        false
    }

    /// Merge two devices, combining keys from both sources
    /// This is important for dual-mode devices that have both Classic and LE keys
    /// 
    /// Special handling for CSRK Counter:
    /// - When merging CSRK keys with the same key value, takes MAX counter
    /// - This prevents counter rollback and protects against replay attacks
    /// - Critical because Windows doesn't persist Counter in registry
    fn merge_devices(system_device: &BluetoothDevice, efi_device: &BluetoothDevice) -> BluetoothDevice {
        // Use base merge as foundation
        let mut merged = system_device.merge_with(efi_device);
        
        // Smart CSRK Counter handling
        if let Some(ref mut merged_le) = merged.le {
            // Merge CSRK Local with MAX Counter preservation
            let csrk_local = match (
                &system_device.le.as_ref().and_then(|le| le.csrk_local.as_ref()),
                &efi_device.le.as_ref().and_then(|le| le.csrk_local.as_ref())
            ) {
                (Some(sys_csrk), Some(efi_csrk)) if sys_csrk.key == efi_csrk.key => {
                    // Same key - take MAX Counter to prevent rollback
                    Some(CsrkKey {
                        key: sys_csrk.key.clone(),
                        counter: sys_csrk.counter.max(efi_csrk.counter),
                        authenticated: sys_csrk.authenticated || efi_csrk.authenticated,
                    })
                }
                (Some(_sys_csrk), Some(efi_csrk)) => {
                    // Different keys - prefer EFI (newer source)
                    Some((*efi_csrk).clone())
                }
                (Some(csrk), None) | (None, Some(csrk)) => Some((*csrk).clone()),
                (None, None) => None,
            };
            
            // Merge CSRK Remote with MAX Counter preservation
            let csrk_remote = match (
                &system_device.le.as_ref().and_then(|le| le.csrk_remote.as_ref()),
                &efi_device.le.as_ref().and_then(|le| le.csrk_remote.as_ref())
            ) {
                (Some(sys_csrk), Some(efi_csrk)) if sys_csrk.key == efi_csrk.key => {
                    Some(CsrkKey {
                        key: sys_csrk.key.clone(),
                        counter: sys_csrk.counter.max(efi_csrk.counter),
                        authenticated: sys_csrk.authenticated || efi_csrk.authenticated,
                    })
                }
                (Some(_sys_csrk), Some(efi_csrk)) => {
                    Some((*efi_csrk).clone())
                }
                (Some(csrk), None) | (None, Some(csrk)) => Some((*csrk).clone()),
                (None, None) => None,
            };
            
            merged_le.csrk_local = csrk_local;
            merged_le.csrk_remote = csrk_remote;
        }
        
        merged
    }

    /// Perform intelligent bidirectional synchronization
    ///
    /// Algorithm:
    /// 1. Read bluevein.json from EFI partition
    /// 2. Read current Bluetooth state from system
    /// 3. MERGE strategy:
    ///    - For each device in EFI:
    ///      * If device does NOT exist in system → SKIP (don't create)
    ///      * If device exists but keys differ → UPDATE keys from EFI (merge both Classic and LE)
    ///    - For each device in system:
    ///      * If it's NOT in EFI → ADD to EFI (new pairing on this OS)
    /// 4. Write updated bluevein.json back to EFI
    pub fn sync_bidirectional(&mut self) -> Result<(), Box<dyn Error>> {
        log!("[BlueVein] Starting bidirectional synchronization...");

        // Read config from EFI (may not exist)
        let efi_config = match efi::read_config() {
            Ok(config) => {
                log!("[BlueVein] Found existing EFI config");
                Some(config)
            }
            Err(efi::EfiError::NotFound) => {
                log!("[BlueVein] No EFI config found, will create from system state");
                None
            }
            Err(e) => {
                log!("[BlueVein] Error reading EFI config: {}", e);
                return Err(Box::new(e));
            }
        };

        // Read current system state
        let mut system_config = BlueVeinConfig::new();
        let adapters = match self.bt_manager.get_adapters() {
            Ok(adapters) => adapters,
            Err(e) => {
                log!("[BlueVein] Error getting adapters: {}", e);
                return Err(e);
            }
        };

        // Build system state map
        for adapter_mac in &adapters {
            match self.bt_manager.get_devices(adapter_mac) {
                Ok(devices) => {
                    if !devices.is_empty() {
                        log!(
                            "[BlueVein] Found {} devices for adapter {}",
                            devices.len(),
                            adapter_mac
                        );
                        let mut device_map = HashMap::new();
                        for device in devices {
                            device_map.insert(device.mac_address.clone(), device);
                        }
                        system_config.set_adapter_devices(adapter_mac.clone(), device_map);
                    }
                }
                Err(e) => {
                    log!(
                        "[BlueVein] Error reading devices for adapter {}: {}",
                        adapter_mac,
                        e
                    );
                }
            }
        }

        // Merge strategy: Update existing devices from EFI, add new system devices to EFI
        let final_config = if let Some(mut efi_cfg) = efi_config {
            log!("[BlueVein] Merging EFI config with system state");

            // Step 1: Apply EFI keys to existing system devices
            for adapter_mac in &adapters {
                if let Some(efi_devices) = efi_cfg.get_adapter_devices(adapter_mac) {
                    if let Some(system_devices) = system_config.get_adapter_devices(adapter_mac) {
                        log!("[BlueVein] Processing adapter {}", adapter_mac);

                        for (device_mac, efi_device) in efi_devices {
                            if let Some(system_device) = system_devices.get(device_mac) {
                                // Device exists in both EFI and system
                                // Merge to combine both Classic and LE keys if needed
                                let merged = Self::merge_devices(system_device, efi_device);
                                
                                if Self::devices_differ(system_device, &merged) {
                                    // Keys differ or missing - update from merged result
                                    log!(
                                        "[BlueVein]   ○ Updating keys for device {} (Classic: {}, LE: {})",
                                        device_mac,
                                        merged.classic.is_some(),
                                        merged.le.is_some()
                                    );
                                    match self.bt_manager.set_device(adapter_mac, &merged) {
                                        Ok(_) => {
                                            log!("[BlueVein]   ✓ Updated device {}", device_mac)
                                        }
                                        Err(e) => log!(
                                            "[BlueVein]   ✗ Failed to update device {}: {}",
                                            device_mac,
                                            e
                                        ),
                                    }
                                } else {
                                    log!(
                                        "[BlueVein]   ✓ Device {} already has correct keys",
                                        device_mac
                                    );
                                }
                            } else {
                                // Device in EFI but NOT in system - don't create it
                                log!("[BlueVein]   ○ Device {} exists in EFI but not in system - skipping (will sync on re-pair)", device_mac);
                            }
                        }
                    }
                }

                // Step 2: Add system devices that are not in EFI
                if let Some(system_devices) = system_config.get_adapter_devices(adapter_mac) {
                    // Collect devices to add (to avoid borrow conflict)
                    let mut devices_to_add = Vec::new();

                    let efi_devices = efi_cfg.get_adapter_devices(adapter_mac);
                    for (device_mac, system_device) in system_devices {
                        let device_in_efi = efi_devices
                            .map(|devices| devices.contains_key(device_mac))
                            .unwrap_or(false);

                        if !device_in_efi {
                            // Device in system but NOT in EFI - add it
                            devices_to_add.push(system_device.clone());
                        }
                    }

                    // Now add collected devices
                    for device in devices_to_add {
                        log!(
                            "[BlueVein]   + Adding new system device {} to EFI (Classic: {}, LE: {})",
                            device.mac_address,
                            device.classic.is_some(),
                            device.le.is_some()
                        );
                        efi_cfg.update_device(adapter_mac.clone(), device);
                    }
                }
            }

            efi_cfg
        } else {
            // No EFI config exists, use system state
            log!("[BlueVein] Creating new EFI config from system state");
            system_config
        };

        // Write merged config back to EFI
        match efi::write_config(&final_config) {
            Ok(_) => log!("[BlueVein] Successfully wrote merged config to EFI"),
            Err(e) => {
                log!("[BlueVein] Error writing config to EFI: {}", e);
                return Err(Box::new(e));
            }
        }

        log!("[BlueVein] Bidirectional synchronization complete");
        Ok(())
    }

    /// Perform initial synchronization from EFI to system
    /// This reads the shared config and updates system Bluetooth keys
    #[allow(dead_code)]
    pub fn sync_from_efi(&mut self) -> Result<(), Box<dyn Error>> {
        log!("[BlueVein] Starting synchronization from EFI...");

        // Read config from EFI
        let config = match efi::read_config() {
            Ok(config) => config,
            Err(efi::EfiError::NotFound) => {
                log!("[BlueVein] No existing config found on EFI, will create on first change");
                return Ok(());
            }
            Err(e) => return Err(Box::new(e)),
        };

        // Get local adapters
        let adapters = self.bt_manager.get_adapters()?;

        // For each adapter, sync devices
        for adapter_mac in adapters {
            if let Some(devices) = config.get_adapter_devices(&adapter_mac) {
                log!(
                    "[BlueVein] Syncing {} devices for adapter {}",
                    devices.len(),
                    adapter_mac
                );

                for (device_mac, device) in devices {
                    match self.bt_manager.set_device(&adapter_mac, device) {
                        Ok(_) => log!("[BlueVein]   ✓ Updated keys for device {}", device_mac),
                        Err(e) => log!(
                            "[BlueVein]   ✗ Failed to update device {}: {}",
                            device_mac,
                            e
                        ),
                    }
                }
            }
        }

        log!("[BlueVein] Synchronization from EFI complete");
        Ok(())
    }

    /// Sync current system state to EFI
    /// This reads system Bluetooth keys and writes them to the shared config
    #[allow(dead_code)]
    pub fn sync_to_efi(&mut self) -> Result<(), Box<dyn Error>> {
        log!("[BlueVein] Syncing current state to EFI...");

        // Read existing config from EFI (or create empty)
        let mut config = match efi::read_config() {
            Ok(config) => config,
            Err(efi::EfiError::NotFound) => BlueVeinConfig::new(),
            Err(e) => return Err(Box::new(e)),
        };

        // Get local adapters
        let adapters = self.bt_manager.get_adapters()?;

        // For each adapter, get devices and update config
        for adapter_mac in adapters {
            let devices = self.bt_manager.get_devices(&adapter_mac)?;

            if !devices.is_empty() {
                log!(
                    "[BlueVein] Found {} devices for adapter {}",
                    devices.len(),
                    adapter_mac
                );

                let mut device_map = HashMap::new();
                for device in devices {
                    device_map.insert(device.mac_address.clone(), device);
                }

                config.set_adapter_devices(adapter_mac, device_map);
            }
        }

        // Write config to EFI
        efi::write_config(&config)?;
        log!("[BlueVein] Successfully synced to EFI");

        Ok(())
    }

    /// Handle a device change event (pairing or key modification)
    ///
    /// Updates the device keys in bluevein.json
    pub fn handle_device_change(
        &mut self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<(), Box<dyn Error>> {
        log!(
            "[BlueVein] Device change detected: {} on adapter {}",
            device_mac,
            adapter_mac
        );

        // Get the device info
        let device = match self.bt_manager.get_device(adapter_mac, device_mac) {
            Ok(dev) => dev,
            Err(e) => {
                log!("[BlueVein] Error getting device info: {}", e);
                return Err(e);
            }
        };

        log!("[BlueVein] Reading existing EFI config...");
        // Read existing config
        let mut config = match efi::read_config() {
            Ok(config) => {
                log!("[BlueVein] Found existing EFI config");
                config
            }
            Err(efi::EfiError::NotFound) => {
                log!("[BlueVein] No EFI config found, creating new");
                BlueVeinConfig::new()
            }
            Err(e) => {
                log!("[BlueVein] Error reading EFI config: {}", e);
                return Err(Box::new(e));
            }
        };

        log!(
            "[BlueVein] Updating device {} (Classic: {}, LE: {})",
            device.mac_address,
            device.classic.is_some(),
            device.le.is_some()
        );
        // Update config
        config.update_device(adapter_mac.to_string(), device.clone());

        log!("[BlueVein] Writing updated config to EFI...");
        // Write back to EFI
        match efi::write_config(&config) {
            Ok(_) => {
                log!(
                    "[BlueVein] ✓ Successfully updated EFI config for device {}",
                    device_mac
                );

                // Verify write
                if let Ok(verify_config) = efi::read_config() {
                    if let Some(stored_device) = verify_config.get_device(adapter_mac, &device.mac_address) {
                        log!(
                            "[BlueVein] ✓ Verified: Device {} is in EFI config",
                            device_mac
                        );
                        if Self::devices_differ(&device, stored_device) {
                            log!("[BlueVein] ✗ Warning: Device keys differ after write!");
                        }
                    } else {
                        log!("[BlueVein] ✗ Warning: Device {} NOT found in EFI config after write!", device_mac);
                    }
                }

                Ok(())
            }
            Err(e) => {
                log!("[BlueVein] ✗ Failed to write EFI config: {}", e);
                Err(Box::new(e))
            }
        }
    }

    /// Handle a device removal event
    ///
    /// Does NOT remove device from bluevein.json because:
    /// - Device may still be paired on another OS
    /// - If user re-pairs on this OS, new key will be synced automatically
    /// - Keeps the shared config as a "union" of all paired devices across both OSes
    pub fn handle_device_removal(
        &mut self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<(), Box<dyn Error>> {
        log!(
            "[BlueVein] Device removal detected: {} on adapter {}",
            device_mac,
            adapter_mac
        );
        log!("[BlueVein] NOT removing from EFI (may be active on other OS)");

        // Don't modify EFI - just log the event
        // The device will remain in bluevein.json and can be used on the other OS
        // If user re-pairs on this OS, the key will be updated automatically

        Ok(())
    }

    /// Check EFI for changes and apply them to the system
    /// This allows changes made by another OS to be detected
    ///
    /// Only updates keys for devices that already exist in the system.
    /// Does NOT create new devices.
    #[allow(dead_code)]
    pub fn check_efi_changes(&mut self) -> Result<(), Box<dyn Error>> {
        // Read config from EFI
        let config = match efi::read_config() {
            Ok(config) => config,
            Err(efi::EfiError::NotFound) => {
                return Ok(());
            }
            Err(e) => return Err(Box::new(e)),
        };

        // Get local adapters
        let adapters = self.bt_manager.get_adapters()?;

        // For each adapter, check for differences and update
        for adapter_mac in adapters {
            if let Some(efi_devices) = config.get_adapter_devices(&adapter_mac) {
                // Get current system devices
                let system_devices = self.bt_manager.get_devices(&adapter_mac)?;
                let system_map: HashMap<String, BluetoothDevice> = system_devices
                    .into_iter()
                    .map(|d| (d.mac_address.clone(), d))
                    .collect();

                // Apply changes from EFI only for devices that exist in system
                for (device_mac, efi_device) in efi_devices {
                    if let Some(system_device) = system_map.get(device_mac) {
                        // Device exists in system - merge and check if keys differ
                        let merged = Self::merge_devices(system_device, efi_device);
                        if Self::devices_differ(system_device, &merged) {
                            log!(
                                "[BlueVein] Key mismatch for {} - updating from EFI",
                                device_mac
                            );
                            self.bt_manager.set_device(&adapter_mac, &merged)?;
                        }
                    }
                    // If device doesn't exist in system - don't create it
                    // User will pair it manually if needed
                }
            }
        }

        Ok(())
    }
}

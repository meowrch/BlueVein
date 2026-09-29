use crate::bluetooth::{BluetoothDevice, BluetoothManager, CsrkKey, PendingDeletion};
use crate::config::BlueVeinConfig;
use crate::efi::{self, ConfigStore, EfiContext};
use crate::log;
use std::collections::{HashMap, HashSet};
use std::error::Error;

/// Bond removals that this run could not finish. Everything else was
/// synchronized and published, the deletion markers survive in EFI, and the
/// next cycle retries them, so a service may keep running after this error.
#[derive(Debug)]
pub struct UnfinishedRemovals(Vec<String>);

impl std::fmt::Display for UnfinishedRemovals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unfinished bond removals: {}", self.0.join("; "))
    }
}

impl Error for UnfinishedRemovals {}

/// Journals keep MAC addresses for years; the last known name makes an entry
/// identifiable. Names are public device identifiers, never key material.
fn describe<const N: usize>(mac: &str, sources: [Option<&BluetoothDevice>; N]) -> String {
    match sources.iter().flatten().find_map(|device| device.name.as_deref()) {
        Some(name) => format!("{} ({})", mac, name),
        None => mac.to_string(),
    }
}

/// Synchronization manager
pub struct SyncManager {
    bt_manager: Box<dyn BluetoothManager>,
    store: Box<dyn ConfigStore>,
}

impl SyncManager {
    #[cfg(test)]
    pub(crate) fn with_test_store(bt_manager: Box<dyn BluetoothManager>, store: Box<dyn ConfigStore>) -> Self {
        Self { bt_manager, store }
    }

    /// Create a new sync manager
    pub fn new(bt_manager: Box<dyn BluetoothManager>, efi_context: EfiContext) -> Self {
        Self {
            bt_manager,
            store: Box::new(efi_context),
        }
    }

    /// Create a new sync manager with default EFI device
    #[allow(dead_code)]
    pub fn with_default_efi(bt_manager: Box<dyn BluetoothManager>) -> Self {
        Self {
            bt_manager,
            store: Box::new(EfiContext::default()),
        }
    }

    /// A successful write must be readable and preserve the complete shared state.
    /// Never log the configuration: it contains pairing secrets.
    fn write_verified(&mut self, config: &BlueVeinConfig) -> Result<(), Box<dyn Error>> {
        self.store.write(config)?;
        if self.store.read()? != *config {
            return Err("EFI read-back does not match the exported configuration".into());
        }
        Ok(())
    }

    /// Merge two devices, combining keys from both sources
    /// This is important for dual-mode devices that have both Classic and LE keys
    ///
    /// Special handling for CSRK Counter:
    /// - When merging CSRK keys with the same key value, takes MAX counter
    /// - This prevents counter rollback and protects against replay attacks
    /// - Critical because Windows doesn't persist Counter in registry
    fn merge_devices(
        system_device: &BluetoothDevice,
        efi_device: &BluetoothDevice,
    ) -> BluetoothDevice {
        // Use base merge as foundation
        let mut merged = system_device.merge_with(efi_device);

        // Smart CSRK Counter handling
        if let Some(ref mut merged_le) = merged.le {
            // Merge CSRK Local with MAX Counter preservation
            let csrk_local = match (
                &system_device
                    .le
                    .as_ref()
                    .and_then(|le| le.csrk_local.as_ref()),
                &efi_device.le.as_ref().and_then(|le| le.csrk_local.as_ref()),
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
                &system_device
                    .le
                    .as_ref()
                    .and_then(|le| le.csrk_remote.as_ref()),
                &efi_device
                    .le
                    .as_ref()
                    .and_then(|le| le.csrk_remote.as_ref()),
            ) {
                (Some(sys_csrk), Some(efi_csrk)) if sys_csrk.key == efi_csrk.key => Some(CsrkKey {
                    key: sys_csrk.key.clone(),
                    counter: sys_csrk.counter.max(efi_csrk.counter),
                    authenticated: sys_csrk.authenticated || efi_csrk.authenticated,
                }),
                (Some(_sys_csrk), Some(efi_csrk)) => Some((*efi_csrk).clone()),
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
    ///      * If absent locally → import when the backend validates a complete bond
    ///      * If device exists but keys differ → UPDATE keys from EFI (merge both Classic and LE)
    ///    - For each device in system:
    ///      * If it's NOT in EFI → ADD to EFI (new pairing on this OS)
    /// 4. Write updated bluevein.json back to EFI
    pub fn sync_bidirectional(&mut self) -> Result<(), Box<dyn Error>> {
        let result = self.sync_bidirectional_mode(true, true);
        let applied = self.bt_manager.apply_pending();
        // A hard failure outranks a removal that the next cycle can retry.
        match result {
            Err(e) if e.downcast_ref::<UnfinishedRemovals>().is_none() => Err(e),
            result => applied.and(result),
        }
    }

    pub fn preview_bidirectional(&mut self) -> Result<(), Box<dyn Error>> {
        self.sync_bidirectional_mode(false, false)
    }

    pub fn repair_efi_only(&mut self) -> Result<(), Box<dyn Error>> {
        self.sync_bidirectional_mode(true, false)
    }

    fn sync_bidirectional_mode(&mut self, apply: bool, allow_local_writes: bool) -> Result<(), Box<dyn Error>> {
        log!(
            "[BlueVein] Starting bidirectional synchronization (EFI device: {})...",
            self.store.display_name()
        );

        // Read config from EFI (may not exist)
        let mut efi_config = match self.store.read() {
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

        let original_config = efi_config.clone();
        if let Some(config) = efi_config.as_mut() {
            self.bt_manager.migrate_shared_config(config)?;
        }
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
                    {
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
                    return Err(format!("Failed to read adapter {}: {}", adapter_mac, e).into());
                }
            }
        }

        // Merge strategy: Update existing devices from EFI, add new system devices to EFI
        // One peer that cannot be unpaired must not hide every other device.
        let mut deferred: Vec<String> = Vec::new();
        let final_config = if let Some(mut efi_cfg) = efi_config {
            log!("[BlueVein] Merging EFI config with system state");

            // Step 1: Apply EFI keys to existing system devices
            for adapter_mac in &adapters {
                let mut merged_devices = Vec::new();
                let mut completed_deletions = HashSet::new();
                if let Some(efi_devices) = efi_cfg.get_adapter_devices(adapter_mac) {
                    if let Some(system_devices) = system_config.get_adapter_devices(adapter_mac) {
                        log!("[BlueVein] Processing adapter {}", adapter_mac);

                        for (device_mac, efi_device) in efi_devices {
                            let local_device = system_devices.get(device_mac);
                            let label = describe(device_mac, [local_device, Some(efi_device)]);
                            if let Some(marker) = &efi_device.pending_deletion {
                                if !matches!(marker.source.as_str(), "linux" | "windows") {
                                    log!("[BlueVein] Unknown deletion marker source for {}; leaving it untouched", label);
                                    continue;
                                }
                                if let Some(local) = local_device {
                                    if marker.comparable_with(local) && !marker.matches_bond(local) {
                                        log!("[BlueVein] New bond {} replaces an older deletion marker", label);
                                        merged_devices.push(local.clone());
                                        continue;
                                    }
                                }
                                if marker.source == self.bt_manager.platform_id() {
                                    // The originating OS has already removed this bond.
                                    continue;
                                }
                                match local_device {
                                    Some(local) if marker.matches_bond(local) => {
                                        if !apply {
                                            log!("[BlueVein] AUDIT would remove matching bond {}", label);
                                            continue;
                                        }
                                        if !allow_local_writes { return Err(format!("EFI-only repair would remove local bond {}", label).into()); }
                                        // A peer that refuses to unpair keeps its marker and is
                                        // reported after the run; other devices still synchronize.
                                        match self.bt_manager.remove_device(adapter_mac, device_mac) {
                                            Ok(()) => {
                                                if self.bt_manager.get_devices(adapter_mac)?.iter()
                                                    .any(|device| device.mac_address == *device_mac) {
                                                    log!("[BlueVein] Local bond {} still exists after removal", label);
                                                    deferred.push(format!("Local bond {} still exists after removal", label));
                                                } else {
                                                    completed_deletions.insert(device_mac.clone());
                                                }
                                            }
                                            Err(e) => {
                                                log!("[BlueVein] Could not remove bond {}: {}", label, e);
                                                deferred.push(format!("Could not remove bond {}: {}", label, e));
                                            }
                                        }
                                    }
                                    Some(_) => {
                                        log!("[BlueVein] Bond {} cannot be compared with deletion marker; leaving both records untouched", label);
                                    }
                                    None => { completed_deletions.insert(device_mac.clone()); }
                                }
                                continue;
                            }
                            if let Some(system_device) = local_device {
                                // Device exists in both EFI and system
                                // Merge to combine both Classic and LE keys if needed
                                let mut merged = Self::merge_devices(system_device, efi_device);
                                merged.name = self.bt_manager.choose_shared_name(system_device, efi_device);
                                let label = describe(device_mac, [Some(&merged)]);

                                if let Some(reason) = self.bt_manager.update_reason(system_device, &merged) {
                                    log!("[BlueVein] Cannot update {}: {}", label, reason);
                                    // An unsafe key import is not a reason to lose a known name.
                                    if merged.name.is_some() && merged.name != efi_device.name {
                                        let mut named = efi_device.clone();
                                        named.name = merged.name.clone();
                                        merged_devices.push(named);
                                    }
                                    continue;
                                }
                                merged_devices.push(merged.clone());
                                if self.bt_manager.needs_update(system_device, &merged) {
                                    // Keys differ or missing - update from merged result
                                    log!(
                                        "[BlueVein]   ○ Updating keys for device {} (Classic: {}, LE: {})",
                                        label,
                                        merged.classic.is_some(),
                                        merged.le.is_some()
                                    );
                                    if !apply {
                                        log!("[BlueVein] AUDIT would update local keys for {}", label);
                                        continue;
                                    }
                                    if !allow_local_writes {
                                        return Err(format!("EFI-only repair would change local keys for {}", label).into());
                                    }
                                    match self.bt_manager.set_device(adapter_mac, &merged) {
                                        Ok(_) => {
                                            log!("[BlueVein]   ✓ Updated device {}", label)
                                        }
                                        Err(e) => return Err(format!(
                                            "Failed to update device {}: {}", label, e
                                        ).into()),
                                    }
                                } else {
                                    log!(
                                        "[BlueVein]   ✓ Device {} already has correct keys",
                                        label
                                    );
                                }
                            } else {
                                self.import_missing(adapter_mac, efi_device, apply, allow_local_writes)?;
                            }
                        }
                    }
                }

                for device in merged_devices {
                    efi_cfg.update_device(adapter_mac.clone(), device);
                }
                for device_mac in &completed_deletions {
                    let label = describe(device_mac, [efi_cfg.get_device(adapter_mac, device_mac)]);
                    efi_cfg.remove_device(adapter_mac, device_mac);
                    log!("[BlueVein] Completed deletion of bond {} on both operating systems", label);
                }
                // Step 2: Add system devices that are not in EFI
                if let Some(system_devices) = system_config.get_adapter_devices(adapter_mac) {
                    // Collect devices to add (to avoid borrow conflict)
                    let mut devices_to_add = Vec::new();

                    let efi_devices = efi_cfg.get_adapter_devices(adapter_mac);
                    for (device_mac, system_device) in system_devices {
                        if completed_deletions.contains(device_mac) { continue; }
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
                            describe(&device.mac_address, [Some(&device)]),
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

        if original_config.as_ref() == Some(&final_config) {
            log!("[BlueVein] Shared config unchanged; skipping EFI write");
        } else if !apply {
            log!("[BlueVein] AUDIT would update shared EFI config; no key writes performed");
        } else {
            // Write merged config back to EFI
            match self.write_verified(&final_config) {
                Ok(_) => log!(
                    "[BlueVein] Successfully wrote merged config to EFI (device: {})",
                    self.store.display_name()
                ),
                Err(e) => {
                    log!("[BlueVein] Error writing config to EFI: {}", e);
                    return Err(e);
                }
            }

            log!("[BlueVein] Bidirectional synchronization complete");
        }
        // Devices that could not be removed keep their marker; report them only
        // after the rest of the synchronization has been published.
        if !deferred.is_empty() {
            return Err(Box::new(UnfinishedRemovals(deferred)));
        }
        Ok(())
    }

    fn import_missing(&mut self, adapter: &str, device: &BluetoothDevice, apply: bool, allow_local_writes: bool) -> Result<(), Box<dyn Error>> {
        let label = describe(&device.mac_address, [Some(device)]);
        if let Some(reason) = self.bt_manager.import_missing_reason(device) {
            log!("[BlueVein] Cannot import {}: {}", label, reason);
            return Ok(());
        }
        if !apply {
            log!("[BlueVein] AUDIT would import missing bond {}", label);
            return Ok(());
        }
        if !allow_local_writes { return Err("EFI-only repair would create a local bond".into()); }
        self.bt_manager.set_device(adapter, device)?;
        log!("[BlueVein] Imported missing bond {}", label);
        Ok(())
    }

    /// Perform initial synchronization from EFI to system
    /// This reads the shared config and updates system Bluetooth keys
    #[allow(dead_code)]
    pub fn sync_from_efi(&mut self) -> Result<(), Box<dyn Error>> {
        self.sync_bidirectional()
    }

    /// Sync current system state to EFI
    /// This reads system Bluetooth keys and writes them to the shared config
    #[allow(dead_code)]
    pub fn sync_to_efi(&mut self) -> Result<(), Box<dyn Error>> {
        log!("[BlueVein] Syncing current state to EFI...");

        // Read existing config from EFI (or create empty)
        let mut config = match self.store.read() {
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
        self.write_verified(&config)?;
        log!(
            "[BlueVein] Successfully synced to EFI (device: {})",
            self.store.display_name()
        );

        Ok(())
    }

    /// Read complete local records, including LE-only devices.
    pub fn local_snapshot(&self) -> Result<HashMap<(String, String), BluetoothDevice>, Box<dyn Error>> {
        let mut snapshot = HashMap::new();
        for adapter in self.bt_manager.get_adapters()? {
            for device in self.bt_manager.get_devices(&adapter)? {
                snapshot.insert((adapter.clone(), device.mac_address.clone()), device);
            }
        }
        Ok(snapshot)
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
        let mut config = match self.store.read() {
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
            describe(&device.mac_address, [Some(&device), config.get_device(adapter_mac, &device.mac_address)]),
            device.classic.is_some(),
            device.le.is_some()
        );
        // Local change wins for represented fields; retain other-platform metadata.
        let device = match config.get_device(adapter_mac, &device.mac_address) {
            Some(shared) if shared.pending_deletion.as_ref().is_some_and(|marker| marker.matches_bond(&device)) => {
                // A cached info-file change is not a new pairing.
                return Ok(());
            }
            Some(shared) if shared.pending_deletion.is_some() => {
                log!("[BlueVein] New bond for {}; cancelling {} deletion marker",
                    describe(&device.mac_address, [Some(&device), Some(shared)]),
                    shared.pending_deletion.as_ref().unwrap().source);
                device
            }
            Some(shared) => Self::merge_devices(shared, &self.bt_manager.prepare_local_export(&device, shared)),
            None => device,
        };
        if config.get_device(adapter_mac, &device.mac_address) == Some(&device) {
            return Ok(());
        }
        config.update_device(adapter_mac.to_string(), device.clone());

        log!("[BlueVein] Writing updated config to EFI...");
        // Write back to EFI
        match self.write_verified(&config) {
            Ok(_) => {
                log!(
                    "[BlueVein] ✓ Successfully updated EFI config for device {} (device: {})",
                    describe(device_mac, [Some(&device)]),
                    self.store.display_name()
                );

                Ok(())
            }
            Err(e) => {
                log!("[BlueVein] ✗ Failed to write EFI config: {}", e);
                Err(e)
            }
        }
    }

    /// Mark a previously synchronized bond for deletion only after it really
    /// disappeared from this OS. The other OS will remove the matching bond.
    pub fn handle_device_removal(
        &mut self,
        adapter_mac: &str,
        device_mac: &str,
        previous: &BluetoothDevice,
    ) -> Result<(), Box<dyn Error>> {
        if self.bt_manager.get_devices(adapter_mac)?.iter()
            .any(|device| device.mac_address.eq_ignore_ascii_case(device_mac)) {
            return Ok(());
        }
        let mut config = self.store.read()?;
        let Some(shared) = config.get_device(adapter_mac, device_mac) else { return Ok(()); };
        if shared.pending_deletion.is_some() || !shared.same_bond_as(previous) {
            return Ok(());
        }
        let mut marked = shared.clone();
        marked.pending_deletion = Some(PendingDeletion::from_observed(self.bt_manager.platform_id(), previous));
        let label = describe(device_mac, [Some(&marked), Some(previous)]);
        config.update_device(adapter_mac.into(), marked);
        self.write_verified(&config)?;
        log!("[BlueVein] Marked synchronized bond {} for deletion on the other OS", label);
        Ok(())
    }

    /// Check EFI for changes and apply them to the system
    /// This allows changes made by another OS to be detected
    ///
    /// Uses the same validated missing-bond import policy as startup.
    #[allow(dead_code)]
    pub fn check_efi_changes(&mut self) -> Result<(), Box<dyn Error>> {
        self.sync_bidirectional()
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::{LeKeys, LeLongTermKey};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct State {
        local: HashMap<String, BluetoothDevice>,
        shared: BlueVeinConfig,
        local_writes: usize,
        local_removals: usize,
        shared_writes: usize,
        fail_local_read: bool,
        allow_missing: bool,
        activations: usize,
        pending: bool,
        fail_local_write: bool,
        discard_shared_write: bool,
        fail_read_after_write: bool,
        fail_removal: bool,
        block_updates: bool,
    }
    struct Backend(Arc<Mutex<State>>);
    impl BluetoothManager for Backend {
        fn import_missing_reason(&self, device: &BluetoothDevice) -> Option<String> {
            if !self.0.lock().unwrap().allow_missing { return Some("existing bond required".into()); }
            if device.le.as_ref().and_then(|le| le.ltk.as_ref()).is_none() { return Some("incomplete bond".into()); }
            None
        }
        fn apply_pending(&mut self) -> Result<(), Box<dyn Error>> {
            let mut state = self.0.lock().unwrap();
            if state.pending { state.activations += 1; state.pending = false; }
            Ok(())
        }
        fn get_adapters(&self) -> Result<Vec<String>, Box<dyn Error>> { Ok(vec!["adapter".into()]) }
        fn get_devices(&self, _: &str) -> Result<Vec<BluetoothDevice>, Box<dyn Error>> {
            let state = self.0.lock().unwrap();
            if state.fail_local_read { return Err("simulated unreadable registry".into()); }
            Ok(state.local.values().cloned().collect())
        }
        fn get_device(&self, _: &str, mac: &str) -> Result<BluetoothDevice, Box<dyn Error>> {
            self.0.lock().unwrap().local.get(mac).cloned().ok_or_else(|| "missing".into())
        }
        fn set_device(&mut self, _: &str, device: &BluetoothDevice) -> Result<(), Box<dyn Error>> {
            let mut state = self.0.lock().unwrap();
            if state.fail_local_write { return Err("simulated failed registry write".into()); }
            state.local.insert(device.mac_address.clone(), device.clone());
            state.local_writes += 1;
            state.pending = true;
            Ok(())
        }
        fn remove_device(&mut self, _: &str, mac: &str) -> Result<(), Box<dyn Error>> {
            let mut state = self.0.lock().unwrap();
            if state.fail_removal { return Err("simulated unpair failure".into()); }
            state.local.remove(mac);
            state.local_removals += 1;
            Ok(())
        }
        fn update_reason(&self, _: &BluetoothDevice, _: &BluetoothDevice) -> Option<String> {
            self.0.lock().unwrap().block_updates.then(|| "simulated unsafe import".to_string())
        }
    }
    struct Store(Arc<Mutex<State>>);
    impl ConfigStore for Store {
        fn read(&self) -> Result<BlueVeinConfig, efi::EfiError> {
            let state = self.0.lock().unwrap();
            if state.fail_read_after_write && state.shared_writes > 0 {
                return Err(efi::EfiError::ReadError("simulated verification read failure".into()));
            }
            Ok(state.shared.clone())
        }
        fn write(&mut self, config: &BlueVeinConfig) -> Result<(), efi::EfiError> {
            let mut state = self.0.lock().unwrap();
            if !state.discard_shared_write { state.shared = config.clone(); }
            state.shared_writes += 1;
            Ok(())
        }
        fn display_name(&self) -> &str { "test memory" }
    }
    fn device(key: &str) -> BluetoothDevice {
        BluetoothDevice::le_with_ltk("phone".into(), LeLongTermKey {
            key: key.repeat(16), authenticated: Some(1), enc_size: Some(16), ediv: Some(0), rand: Some(0),
        })
    }
    fn setup(local: BluetoothDevice, shared: BluetoothDevice) -> (SyncManager, Arc<Mutex<State>>) {
        let mut state = State::default();
        state.local.insert(local.mac_address.clone(), local);
        state.shared.update_device("adapter".into(), shared);
        let state = Arc::new(Mutex::new(state));
        (SyncManager { bt_manager: Box::new(Backend(state.clone())), store: Box::new(Store(state.clone())) }, state)
    }

    #[test]
    fn observed_removal_marks_efi_then_other_os_deletes_matching_bond() {
        let old = device("11");
        let (mut source, state) = setup(old.clone(), old.clone());
        state.lock().unwrap().local.clear();
        source.handle_device_removal("adapter", "phone", &old).unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.shared.get_device("adapter", "phone").unwrap().pending_deletion.as_ref().unwrap().source, "test");
            assert_eq!(state.local_removals, 0);
        }
        // The originating OS must not import its own pending deletion.
        state.lock().unwrap().allow_missing = true;
        source.sync_bidirectional().unwrap();
        assert!(state.lock().unwrap().local.is_empty());

        let mut shared = state.lock().unwrap().shared.get_device("adapter", "phone").unwrap().clone();
        shared.pending_deletion.as_mut().unwrap().source = "windows".into();
        let (mut other, other_state) = setup(old, shared);
        other.preview_bidirectional().unwrap();
        assert_eq!(other_state.lock().unwrap().local_removals, 0);
        other.sync_bidirectional().unwrap();
        let state = other_state.lock().unwrap();
        assert_eq!(state.local_removals, 1);
        assert!(!state.local.contains_key("phone"));
        assert!(state.shared.get_device("adapter", "phone").is_none());
    }

    #[test]
    fn failed_unpair_keeps_its_marker_without_hiding_the_other_devices() {
        let mut shared = device("11");
        shared.pending_deletion = Some(PendingDeletion::from_observed("windows", &shared));
        let (mut sync, state) = setup(device("11"), shared);
        {
            let mut s = state.lock().unwrap();
            s.fail_removal = true;
            let mut other = device("33"); other.mac_address = "headphones".into();
            s.local.insert(other.mac_address.clone(), other);
        }
        let error = sync.sync_bidirectional().unwrap_err().to_string();
        assert!(error.contains("phone"), "{error}");
        let s = state.lock().unwrap();
        // The bond is still paired here, so the request must survive the run.
        assert!(s.local.contains_key("phone"));
        assert!(s.shared.get_device("adapter", "phone").unwrap().pending_deletion.is_some());
        // An unrelated new pairing is still exported instead of being lost.
        assert!(s.shared.get_device("adapter", "headphones").is_some());
        assert_eq!(s.shared_writes, 1);
    }

    #[test]
    fn existing_shared_record_gains_a_name_discovered_later() {
        let mut local = device("11");
        local.name = Some("IINE GAMEPAD".into());
        let (mut sync, state) = setup(local.clone(), device("11"));
        sync.sync_bidirectional().unwrap();
        {
            let s = state.lock().unwrap();
            assert_eq!(s.shared.get_device("adapter", "phone"), Some(&local));
            // A name is not a key change: the local bond is left alone.
            assert_eq!(s.local_writes, 0);
            assert_eq!(s.shared_writes, 1);
        }
        sync.check_efi_changes().unwrap();
        assert_eq!(state.lock().unwrap().shared_writes, 1);
    }

    #[test]
    fn blocked_key_import_still_publishes_the_local_device_name() {
        let mut local = device("22");
        local.name = Some("IINE GAMEPAD".into());
        let (mut sync, state) = setup(local, device("11"));
        state.lock().unwrap().block_updates = true;
        sync.sync_bidirectional().unwrap();
        {
            let s = state.lock().unwrap();
            let exported = s.shared.get_device("adapter", "phone").unwrap();
            assert_eq!(exported.name.as_deref(), Some("IINE GAMEPAD"));
            // Names travel; the rejected key material does not.
            assert_eq!(exported.le, device("11").le);
            assert_eq!(s.local_writes, 0);
            assert_eq!(s.shared_writes, 1);
        }
        // A name that is already shared must not rewrite EFI on every cycle.
        sync.check_efi_changes().unwrap();
        assert_eq!(state.lock().unwrap().shared_writes, 1);
    }

    #[test]
    fn deletion_marker_does_not_remove_repaired_bond_at_same_address() {
        let mut shared = device("11");
        shared.pending_deletion = Some(PendingDeletion::from_observed("windows", &shared));
        let (mut sync, state) = setup(device("22"), shared.clone());
        sync.sync_bidirectional().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.local_removals, 0);
        assert_eq!(state.local.get("phone"), Some(&device("22")));
        assert_eq!(state.shared.get_device("adapter", "phone"), Some(&device("22")));
    }

    #[test]
    fn other_os_already_absent_clears_marker_without_local_write() {
        let mut shared = device("11");
        shared.pending_deletion = Some(PendingDeletion::from_observed("windows", &shared));
        let (mut sync, state) = setup(device("22"), shared);
        state.lock().unwrap().local.clear();
        sync.sync_bidirectional().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.local_removals, 0);
        assert!(state.shared.get_device("adapter", "phone").is_none());
    }

    #[test]
    fn deletion_requires_observed_local_bond_matching_shared_record() {
        let (mut sync, state) = setup(device("22"), device("11"));
        state.lock().unwrap().local.clear();
        sync.handle_device_removal("adapter", "phone", &device("22")).unwrap();
        let state = state.lock().unwrap();
        assert!(state.shared.get_device("adapter", "phone").unwrap().pending_deletion.is_none());
        assert_eq!(state.shared_writes, 0);
    }

    #[test]
    fn empty_adapter_imports_multiple_bonds_once_and_converges() {
        let (mut sync, state) = setup(device("22"), device("11"));
        {
            let mut s = state.lock().unwrap(); s.local.clear(); s.allow_missing = true;
            for name in ["keyboard1", "keyboard2", "mouse1", "mouse2"] {
                let mut d = device("33"); d.mac_address = name.into(); s.shared.update_device("adapter".into(), d);
            }
            let mut foreign = device("44"); foreign.mac_address = "other-host".into();
            s.shared.update_device("other-adapter".into(), foreign);
        }
        sync.sync_bidirectional().unwrap();
        sync.check_efi_changes().unwrap();
        let s = state.lock().unwrap();
        assert_eq!(s.local.len(), 5);
        assert_eq!(s.local_writes, 5);
        assert_eq!(s.activations, 1);
        assert_eq!(s.shared_writes, 0);
        assert!(!s.local.contains_key("other-host"));
    }

    #[test]
    fn missing_bond_preview_and_efi_only_repair_do_not_write() {
        let (mut sync, state) = setup(device("22"), device("11"));
        { let mut s = state.lock().unwrap(); s.local.clear(); s.allow_missing = true; }
        sync.preview_bidirectional().unwrap();
        assert!(sync.repair_efi_only().is_err());
        let s = state.lock().unwrap();
        assert_eq!(s.local_writes, 0); assert_eq!(s.shared_writes, 0); assert_eq!(s.activations, 0);
    }

    #[test]
    fn incomplete_missing_bond_does_not_block_other_devices_or_delete_shared_record() {
        let (mut sync, state) = setup(device("22"), device("11"));
        { let mut s = state.lock().unwrap(); s.local.clear(); s.allow_missing = true;
          s.shared.update_device("adapter".into(), BluetoothDevice { mac_address: "incomplete".into(), name: None, pending_deletion: None, classic: None, le: Some(LeKeys::default()) }); }
        sync.sync_bidirectional().unwrap();
        let s = state.lock().unwrap();
        assert_eq!(s.local.len(), 1); assert_eq!(s.activations, 1);
        assert!(s.shared.get_device("adapter", "incomplete").is_some());
    }

    #[test]
    fn backend_without_bond_creation_keeps_missing_record_shared_only() {
        let (mut sync, state) = setup(device("22"), device("11"));
        state.lock().unwrap().local.clear();
        sync.sync_bidirectional().unwrap();
        let s = state.lock().unwrap(); assert!(s.local.is_empty()); assert_eq!(s.activations, 0);
    }

    #[test]
    fn lost_efi_export_fails_and_can_be_retried_before_import() {
        let (mut sync, state) = setup(device("22"), device("11"));
        state.lock().unwrap().discard_shared_write = true;
        assert!(sync.handle_device_change("adapter", "phone").is_err());
        assert_eq!(state.lock().unwrap().local_writes, 0);
        state.lock().unwrap().discard_shared_write = false;
        sync.handle_device_change("adapter", "phone").unwrap();
        sync.check_efi_changes().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.local_writes, 0);
        assert_eq!(state.shared.get_device("adapter", "phone"), Some(&device("22")));
    }

    #[test]
    fn unreadable_efi_after_export_is_an_error() {
        let (mut sync, state) = setup(device("22"), device("11"));
        state.lock().unwrap().fail_read_after_write = true;
        assert!(sync.handle_device_change("adapter", "phone").is_err());
        assert_eq!(state.lock().unwrap().local_writes, 0);
    }

    #[test]
    fn startup_rejects_lost_shared_write() {
        let mut local = device("22");
        local.le.as_mut().unwrap().irk = Some("33".repeat(16));
        let shared = BluetoothDevice { mac_address: "phone".into(), name: None, pending_deletion: None, classic: None,
            le: Some(LeKeys { irk: Some("33".repeat(16)), ..Default::default() }) };
        let (mut sync, state) = setup(local, shared);
        state.lock().unwrap().discard_shared_write = true;
        assert!(sync.sync_bidirectional().is_err());
        assert_eq!(state.lock().unwrap().local_writes, 0);
    }

    #[test]
    fn efi_only_repair_refuses_any_local_key_change() {
        let (mut sync, state) = setup(device("11"), device("22"));
        assert!(sync.repair_efi_only().is_err());
        let state = state.lock().unwrap();
        assert_eq!(state.local_writes, 0);
        assert_eq!(state.shared_writes, 0);
        assert_eq!(state.local.get("phone"), Some(&device("11")));
    }

    #[test]
    fn preview_uses_import_plan_without_writing_local_or_shared_keys() {
        let (mut sync, state) = setup(device("11"), device("22"));
        let before = state.lock().unwrap().shared.clone();
        sync.preview_bidirectional().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.local.get("phone"), Some(&device("11")));
        assert_eq!(state.shared, before);
        assert_eq!(state.local_writes, 0);
        assert_eq!(state.shared_writes, 0);
    }

    #[test]
    fn failed_local_read_does_not_publish_a_partial_shared_state() {
        let (mut sync, state) = setup(device("11"), device("22"));
        state.lock().unwrap().fail_local_read = true;
        assert!(sync.sync_bidirectional().is_err());
        let state = state.lock().unwrap();
        assert_eq!(state.shared_writes, 0);
        assert_eq!(state.local_writes, 0);
    }

    #[test]
    fn failed_local_import_is_not_published_as_a_successful_merge() {
        let (mut sync, state) = setup(device("11"), device("22"));
        state.lock().unwrap().fail_local_write = true;
        assert!(sync.sync_bidirectional().is_err());
        let state = state.lock().unwrap();
        assert_eq!(state.shared_writes, 0);
        assert_eq!(state.local_writes, 0);
        assert_eq!(state.local.get("phone"), Some(&device("11")));
    }

    #[test]
    fn unchanged_startup_and_notifications_do_not_write_efi() {
        let (mut sync, state) = setup(device("11"), device("11"));
        sync.sync_bidirectional().unwrap();
        sync.handle_device_change("adapter", "phone").unwrap();
        sync.check_efi_changes().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.shared_writes, 0);
        assert_eq!(state.local_writes, 0);
    }

    #[test]
    fn local_rekey_is_exported_and_is_not_reverted_by_next_import() {
        let (mut sync, state) = setup(device("22"), device("11"));
        sync.handle_device_change("adapter", "phone").unwrap();
        sync.check_efi_changes().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.shared.get_device("adapter", "phone"), Some(&device("22")));
        assert_eq!(state.local_writes, 0);
        assert_eq!(state.shared_writes, 1);
    }

    #[test]
    fn local_export_preserves_other_platform_fields_and_other_devices() {
        let mut shared = device("11");
        shared.le.as_mut().unwrap().peripheral_ltk = shared.le.as_ref().unwrap().ltk.clone();
        shared.le.as_mut().unwrap().address_type = Some("public".into());
        let (mut sync, state) = setup(device("22"), shared.clone());
        let mut other = device("33"); other.mac_address = "headphones".into();
        state.lock().unwrap().shared.update_device("adapter".into(), other.clone());
        sync.handle_device_change("adapter", "phone").unwrap();
        let state = state.lock().unwrap();
        let exported = state.shared.get_device("adapter", "phone").unwrap().le.as_ref().unwrap();
        assert_eq!(exported.ltk, device("22").le.unwrap().ltk);
        assert_eq!(exported.peripheral_ltk, shared.le.unwrap().peripheral_ltk);
        assert_eq!(exported.address_type.as_deref(), Some("public"));
        assert_eq!(state.shared.get_device("adapter", "headphones"), Some(&other));
    }

    #[test]
    fn startup_exports_missing_ltk_for_existing_irk_only_shared_device() {
        let mut local = device("22"); local.le.as_mut().unwrap().irk = Some("33".repeat(16));
        let shared = BluetoothDevice { mac_address: "phone".into(), name: None, pending_deletion: None, classic: None,
            le: Some(LeKeys { irk: Some("33".repeat(16)), ..Default::default() }) };
        let (mut sync, state) = setup(local.clone(), shared);
        sync.sync_bidirectional().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.shared.get_device("adapter", "phone"), Some(&local));
        assert_eq!(state.shared_writes, 1);
        assert_eq!(state.local_writes, 0);
    }
}

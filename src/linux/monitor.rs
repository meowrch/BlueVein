use crate::log;
use crate::sync::SyncManager;
use futures::StreamExt;
use inotify::{EventMask, EventOwned, Inotify, WatchDescriptor, WatchMask, Watches};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const BLUETOOTH_LIB_PATH: &str = "/var/lib/bluetooth";
const EFI_POLL_INTERVAL: Duration = Duration::from_secs(30);
const REMOVAL_SETTLE_TIME: Duration = Duration::from_secs(2);
type Snapshot = HashMap<(String, String), crate::bluetooth::BluetoothDevice>;

fn device_watch_mask() -> WatchMask {
    WatchMask::MODIFY | WatchMask::CREATE | WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO
}

fn is_info_write(mask: EventMask) -> bool {
    mask.intersects(EventMask::MODIFY | EventMask::CLOSE_WRITE | EventMask::MOVED_TO)
}

pub async fn monitor_bluetooth_changes(
    mut sync_manager: SyncManager,
) -> Result<(), Box<dyn Error>> {
    let inotify = Inotify::init()?;
    let mut watch_control = inotify.watches();
    let mut watches = HashMap::new();

    // Watch main bluetooth directory
    let main_watch = watch_control.add(
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
                    if let Ok(watch) = watch_control.add(
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
                    add_device_watches(&mut watch_control, &mut watches, &path);
                }
            }
        }
    }

    log!(
        "[BlueVein] Monitoring {} for Bluetooth changes...",
        BLUETOOTH_LIB_PATH
    );

    // Only a bond observed after a successful startup sync can generate a
    // deletion marker. A record merely absent at boot is not evidence.
    let mut known = sync_manager.local_snapshot()?;

    let mut events = inotify.into_event_stream([0; 4096])?;
    let mut poll = tokio::time::interval(EFI_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // `interval` ticks immediately once; startup already performed a full sync.
    poll.tick().await;
    let mut refresh_after_import = false;
    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else {
                    return Err("Linux Bluetooth event stream ended".into());
                };
                if refresh_after_import {
                    match sync_manager.local_snapshot() {
                        Ok(snapshot) => {
                            known = snapshot;
                            refresh_after_import = false;
                            log!("[BlueVein] Local snapshot recovered after EFI import");
                        }
                        Err(e) => {
                            log!("[BlueVein] Local snapshot still unavailable after EFI import: {}", e);
                            continue;
                        }
                    }
                }
                handle_event(
                    event?,
                    &mut watch_control,
                    &mut watches,
                    &mut sync_manager,
                    &mut known,
                ).await;
            }
            _ = poll.tick() => {
                poll_efi(&mut sync_manager, &mut known, &mut refresh_after_import).await;
            }
        }
    }
}

async fn poll_efi(
    sync_manager: &mut SyncManager,
    known: &mut Snapshot,
    refresh_after_import: &mut bool,
) {
    if *refresh_after_import {
        match sync_manager.local_snapshot() {
            Ok(snapshot) => {
                *known = snapshot;
                *refresh_after_import = false;
                log!("[BlueVein] Local snapshot recovered after EFI import");
            }
            Err(e) => {
                log!("[BlueVein] Local snapshot still unavailable after EFI import: {}", e);
                return;
            }
        }
    }

    let mut current = match sync_manager.local_snapshot() {
        Ok(snapshot) => snapshot,
        Err(e) => {
            log!("[BlueVein] Cannot poll EFI while the local snapshot is unavailable: {}", e);
            return;
        }
    };

    let missing: Vec<_> = known
        .iter()
        .filter(|(id, _)| !current.contains_key(*id))
        .map(|(id, device)| (id.clone(), device.clone()))
        .collect();
    if !missing.is_empty() {
        tokio::time::sleep(REMOVAL_SETTLE_TIME).await;
        current = match sync_manager.local_snapshot() {
            Ok(snapshot) => snapshot,
            Err(e) => {
                log!("[BlueVein] Cannot confirm local removals before EFI import: {}", e);
                return;
            }
        };
        for ((adapter, mac), previous) in missing {
            if current.contains_key(&(adapter.clone(), mac.clone())) {
                continue;
            }
            if let Err(e) = sync_manager.handle_device_removal(&adapter, &mac, &previous) {
                log!("[BlueVein] Failed to mark device removal before EFI import: {}", e);
                return;
            }
        }
    }

    let changed: Vec<_> = current
        .iter()
        .filter(|(id, device)| known.get(*id) != Some(*device))
        .map(|(id, _)| id.clone())
        .collect();
    for (adapter, device) in changed {
        if let Err(e) = sync_manager.handle_device_change(&adapter, &device) {
            log!("[BlueVein] Local export failed; deferring EFI import: {}", e);
            return;
        }
    }
    *known = current;

    if let Err(e) = sync_manager.check_efi_changes() {
        log!("[BlueVein] Periodic EFI import failed: {}", e);
    }
    // Even a synchronization that reports unfinished removals can have changed
    // local records. Never interpret those writes as fresh local pairings.
    *refresh_after_import = true;
    match sync_manager.local_snapshot() {
        Ok(snapshot) => {
            *known = snapshot;
            *refresh_after_import = false;
        }
        Err(e) => log!("[BlueVein] Local snapshot failed after EFI import: {}", e),
    }
}

async fn handle_event(
    event: EventOwned,
    watch_control: &mut Watches,
    watches: &mut HashMap<WatchDescriptor, PathBuf>,
    sync_manager: &mut SyncManager,
    known: &mut Snapshot,
) {
    let Some(name) = event.name else { return; };
    let name_str = name.to_string_lossy().to_string();
    let Some(base_path) = watches.get(&event.wd).cloned() else { return; };
    let full_path = base_path.join(&name_str);

    if base_path.to_str() == Some(BLUETOOTH_LIB_PATH) {
        if name_str.contains(':')
            && name_str.len() == 17
            && event.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
        {
            if let Ok(watch) = watch_control.add(
                &full_path,
                WatchMask::CREATE
                    | WatchMask::DELETE
                    | WatchMask::MODIFY
                    | WatchMask::MOVED_TO
                    | WatchMask::MOVED_FROM,
            ) {
                watches.insert(watch, full_path.clone());
                log!("[BlueVein] New adapter detected: {}", name_str);
                add_device_watches(watch_control, watches, &full_path);
            }
        }
        return;
    }

    if name_str == "info" {
        if !is_info_write(event.mask) { return; }
        let Some(device_mac) = base_path.file_name().and_then(|n| n.to_str()) else { return; };
        let Some(adapter_path) = base_path.parent() else { return; };
        let Some(adapter_mac) = adapter_path.file_name().and_then(|n| n.to_str()) else { return; };
        log!(
            "[BlueVein] Info file updated for device {} on adapter {}",
            device_mac,
            adapter_mac
        );
        if has_pairing_keys(&full_path) {
            log!("[BlueVein] Pairing keys detected, syncing...");
            if let Err(e) = sync_manager.handle_device_change(adapter_mac, device_mac) {
                log!("[BlueVein] Failed to sync device: {}", e);
            } else if let Ok(snapshot) = sync_manager.local_snapshot() {
                if let Some(device) = snapshot.get(&(adapter_mac.to_string(), device_mac.to_string())) {
                    known.insert(
                        (adapter_mac.to_string(), device_mac.to_string()),
                        device.clone(),
                    );
                }
            }
        }
        return;
    }

    if !name_str.contains(':') || name_str.len() != 17 { return; }
    let adapter_mac = base_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if event.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
        log!(
            "[BlueVein] Device removal detected: {} on adapter {}",
            name_str,
            adapter_mac
        );
        let id = (adapter_mac.to_string(), name_str.clone());
        if let Some(previous) = known.get(&id).cloned() {
            tokio::time::sleep(REMOVAL_SETTLE_TIME).await;
            if !full_path.exists() {
                match sync_manager.handle_device_removal(adapter_mac, &name_str, &previous) {
                    Ok(()) => {
                        known.remove(&id);
                    }
                    Err(e) => log!("[BlueVein] Failed to mark device removal: {}", e),
                }
            }
        }
    } else if event.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO) {
        log!(
            "[BlueVein] New device directory detected: {} on adapter {}",
            name_str,
            adapter_mac
        );
        add_device_watches(watch_control, watches, &base_path);
    }
}

/// Add watches for device directories and their info files
fn add_device_watches(
    watch_control: &mut Watches,
    watches: &mut HashMap<WatchDescriptor, PathBuf>,
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
                    if let Ok(watch) = watch_control.add(&device_path, device_watch_mask()) {
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
                    "[LinkKey]"
                    | "[LongTermKey]"
                    | "[PeripheralLongTermKey]"
                    | "[IdentityResolvingKey]"
                    | "[SlaveLongTermKey]" => {
                        let key_value = trimmed.strip_prefix("Key=").unwrap_or("");
                        // Validate key is not empty and has valid hex format (32 chars = 128-bit)
                        if key_value.len() == 32 && key_value.chars().all(|c| c.is_ascii_hexdigit())
                        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::{BluetoothDevice, BluetoothManager, LeLongTermKey};
    use crate::config::BlueVeinConfig;
    use crate::efi::{ConfigStore, EfiError};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct State {
        local: HashMap<String, BluetoothDevice>,
        shared: BlueVeinConfig,
        local_writes: usize,
        shared_writes: usize,
    }

    struct Backend(Arc<Mutex<State>>);
    impl BluetoothManager for Backend {
        fn platform_id(&self) -> &'static str { "linux" }
        fn import_missing_reason(&self, _: &BluetoothDevice) -> Option<String> { None }
        fn get_adapters(&self) -> Result<Vec<String>, Box<dyn Error>> {
            Ok(vec!["adapter".into()])
        }
        fn get_devices(&self, _: &str) -> Result<Vec<BluetoothDevice>, Box<dyn Error>> {
            Ok(self.0.lock().unwrap().local.values().cloned().collect())
        }
        fn get_device(&self, _: &str, mac: &str) -> Result<BluetoothDevice, Box<dyn Error>> {
            self.0
                .lock()
                .unwrap()
                .local
                .get(mac)
                .cloned()
                .ok_or_else(|| "missing test device".into())
        }
        fn set_device(&mut self, _: &str, device: &BluetoothDevice) -> Result<(), Box<dyn Error>> {
            let mut state = self.0.lock().unwrap();
            state.local.insert(device.mac_address.clone(), device.clone());
            state.local_writes += 1;
            Ok(())
        }
        fn remove_device(&mut self, _: &str, mac: &str) -> Result<(), Box<dyn Error>> {
            self.0.lock().unwrap().local.remove(mac);
            Ok(())
        }
    }

    struct Store(Arc<Mutex<State>>);
    impl ConfigStore for Store {
        fn read(&self) -> Result<BlueVeinConfig, EfiError> {
            Ok(self.0.lock().unwrap().shared.clone())
        }
        fn write(&mut self, config: &BlueVeinConfig) -> Result<(), EfiError> {
            let mut state = self.0.lock().unwrap();
            state.shared = config.clone();
            state.shared_writes += 1;
            Ok(())
        }
        fn display_name(&self) -> &str { "test memory" }
    }

    fn device(key: &str) -> BluetoothDevice {
        BluetoothDevice::le_with_ltk(
            "phone".into(),
            LeLongTermKey {
                key: key.repeat(16),
                authenticated: Some(1),
                enc_size: Some(16),
                ediv: Some(0),
                rand: Some(0),
            },
        )
    }

    fn manager(state: Arc<Mutex<State>>) -> SyncManager {
        SyncManager::with_test_store(
            Box::new(Backend(state.clone())),
            Box::new(Store(state)),
        )
    }

    #[test]
    fn atomic_info_rename_is_watched_and_classified_as_a_write() {
        let root = std::env::temp_dir().join(format!(
            "bluevein-inotify-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let mut inotify = Inotify::init().unwrap();
        inotify.watches().add(&root, device_watch_mask()).unwrap();
        let temporary = root.join(".bluevein-info.test.tmp");
        fs::write(&temporary, "[LongTermKey]\nKey=11111111111111111111111111111111\n")
            .unwrap();
        fs::rename(&temporary, root.join("info")).unwrap();

        let mut buffer = [0; 4096];
        let events: Vec<_> = inotify
            .read_events_blocking(&mut buffer)
            .unwrap()
            .map(|event| (event.name.map(|name| name.to_os_string()), event.mask))
            .collect();
        assert!(events.iter().any(|(name, mask)| {
            name.as_deref() == Some(std::ffi::OsStr::new("info"))
                && mask.contains(EventMask::MOVED_TO)
                && is_info_write(*mask)
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn periodic_poll_imports_shared_only_bond_without_inotify_activity() {
        let state = Arc::new(Mutex::new(State::default()));
        state
            .lock()
            .unwrap()
            .shared
            .update_device("adapter".into(), device("11"));
        let mut sync = manager(state.clone());
        let mut known = Snapshot::new();
        let mut refresh = false;

        poll_efi(&mut sync, &mut known, &mut refresh).await;

        let state = state.lock().unwrap();
        assert_eq!(state.local.get("phone"), Some(&device("11")));
        assert_eq!(state.local_writes, 1);
        assert!(known.contains_key(&("adapter".into(), "phone".into())));
        assert!(!refresh);
    }

    #[tokio::test]
    async fn periodic_poll_exports_local_rekey_before_importing_efi() {
        let old = device("11");
        let renewed = device("22");
        let mut initial = State::default();
        initial.local.insert("phone".into(), renewed.clone());
        initial.shared.update_device("adapter".into(), old.clone());
        let state = Arc::new(Mutex::new(initial));
        let mut sync = manager(state.clone());
        let mut known = Snapshot::from([(("adapter".into(), "phone".into()), old)]);
        let mut refresh = false;

        poll_efi(&mut sync, &mut known, &mut refresh).await;

        let state = state.lock().unwrap();
        assert_eq!(state.shared.get_device("adapter", "phone"), Some(&renewed));
        assert_eq!(state.local.get("phone"), Some(&renewed));
        assert_eq!(state.local_writes, 0);
        assert_eq!(state.shared_writes, 1);
    }
}

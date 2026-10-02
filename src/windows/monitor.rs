use crate::bluetooth::BluetoothDevice;
use crate::log;
use crate::sync::SyncManager;
use std::collections::HashMap;
use std::error::Error;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::thread;
use std::time::{Duration, Instant};

type Snapshot = HashMap<(String, String), BluetoothDevice>;

pub(super) fn changed_devices(old: &Snapshot, new: &Snapshot) -> Vec<(String, String)> {
    new.iter().filter(|(id, device)| old.get(*id) != Some(*device))
        .map(|(id, _)| id.clone()).collect()
}

/// Serialize local exports and EFI imports. A separate importer could overwrite
/// a newly paired LE key before the local monitor publishes it.
pub fn monitor_bluetooth_changes(
    mut sync_manager: SyncManager,
    running: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
    log!("[BlueVein] Monitoring Classic and LE records");
    let mut previous = sync_manager.local_snapshot()?;
    let mut missing: HashMap<(String, String), (BluetoothDevice, Instant)> = HashMap::new();
    let mut last_import = Instant::now();
    let mut snapshot_error: Option<(String, Instant)> = None;
    let mut refresh_after_import = false;
    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));
        let current = match sync_manager.local_snapshot() {
            Ok(state) => {
                if snapshot_error.take().is_some() { log!("[BlueVein] Local snapshot recovered"); }
                state
            },
            Err(e) => {
                let message = e.to_string();
                if snapshot_error.as_ref().map_or(true, |(old, since)| old != &message || since.elapsed() >= Duration::from_secs(60)) {
                    log!("[BlueVein] Local snapshot failed (unchanged errors repeat at most once per minute): {}", message);
                    snapshot_error = Some((message, Instant::now()));
                }
                continue;
            }
        };
        if refresh_after_import {
            // An import may have changed or removed local records. When the
            // immediate post-import read failed, the next healthy snapshot is
            // a new baseline, not a set of locally initiated changes.
            previous = current;
            refresh_after_import = false;
            continue;
        }
        let mut exported = true;
        for (adapter, device) in changed_devices(&previous, &current) {
            if let Err(e) = sync_manager.handle_device_change(&adapter, &device) {
                log!("[BlueVein] Local export failed; deferring EFI import: {}", e);
                exported = false;
            }
        }
        if !exported { continue; }
        for (id, old_device) in &previous {
            if !current.contains_key(id) {
                missing.entry(id.clone()).or_insert_with(|| (old_device.clone(), Instant::now()));
            }
        }
        missing.retain(|id, _| !current.contains_key(id));
        let ready: Vec<_> = missing.iter()
            .filter(|(_, (_, since))| since.elapsed() >= Duration::from_secs(3))
            .map(|(id, (device, _))| (id.clone(), device.clone())).collect();
        for ((adapter, mac), old_device) in ready {
            match sync_manager.handle_device_removal(&adapter, &mac, &old_device) {
                Ok(()) => { missing.remove(&(adapter, mac)); }
                Err(e) => log!("[BlueVein] Deferred deletion marker for {}: {}", mac, e),
            }
        }
        previous = current;
        if last_import.elapsed() >= Duration::from_secs(30) {
            if let Err(e) = sync_manager.check_efi_changes() {
                log!("[BlueVein] EFI import failed: {}", e);
            }
            // Imported writes must not be mistaken for new local pairings.
            refresh_after_import = true;
            match sync_manager.local_snapshot() {
                Ok(state) => {
                    if snapshot_error.take().is_some() {
                        log!("[BlueVein] Local snapshot recovered");
                    }
                    previous = state;
                    refresh_after_import = false;
                }
                Err(e) => {
                    let message = e.to_string();
                    if snapshot_error.as_ref().map_or(true, |(old, since)| {
                        old != &message || since.elapsed() >= Duration::from_secs(60)
                    }) {
                        log!("[BlueVein] Local snapshot failed after EFI import (unchanged errors repeat at most once per minute): {}", message);
                        snapshot_error = Some((message, Instant::now()));
                    }
                }
            }
            last_import = Instant::now();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::{LeKeys, LeLongTermKey};

    fn snapshot(key: &str) -> Snapshot {
        let device = BluetoothDevice::le_with_ltk("AA:BB:CC:DD:EE:FF".into(), LeLongTermKey {
            key: key.repeat(16), authenticated: Some(1), enc_size: Some(16), ediv: Some(0), rand: Some(0),
        });
        HashMap::from([(("00:11:22:33:44:55".into(), device.mac_address.clone()), device)])
    }

    #[test]
    fn detects_le_only_pairing_and_rekey_without_classic_changes() {
        let old = snapshot("11");
        assert_eq!(changed_devices(&Snapshot::new(), &old).len(), 1);
        assert_eq!(changed_devices(&old, &snapshot("22")).len(), 1);
        assert!(changed_devices(&old, &old).is_empty());
    }

    #[test]
    fn detects_irk_change_with_unchanged_classic_key() {
        let mut old = snapshot("11");
        let device = old.values_mut().next().unwrap();
        device.classic = Some(crate::bluetooth::ClassicKeys::new("33".repeat(16)));
        device.le = Some(LeKeys { irk: Some("44".repeat(16)), ..Default::default() });
        let mut new = old.clone();
        new.values_mut().next().unwrap().le.as_mut().unwrap().irk = Some("55".repeat(16));
        assert_eq!(changed_devices(&old, &new).len(), 1);
    }
}

mod bluetooth;
mod monitor;

use crate::efi::EfiContext;
use crate::log;
use crate::sync::SyncManager;
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    log!("[BlueVein] Starting Linux service...");

    // Check if we have root permissions
    if !nix::unistd::Uid::effective().is_root() {
        log!("[BlueVein] ERROR: Must run as root!");
        log!("[BlueVein] Please run with: sudo ./bluevein");
        return Err("Requires root privileges".into());
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        if args.len() != 1 || !matches!(args[0].as_str(), "--audit-sync" | "--sync-once") {
            return Err("Usage: bluevein [--audit-sync|--sync-once]".into());
        }
        if args[0] == "--sync-once" {
            log!("[BlueVein] Reconciling the Bluetooth service before synchronization...");
            bluetooth::LinuxBluetoothManager::ensure_bluetooth_running()?;
        }
        let efi_context = EfiContext::from_env();
        efi_context.validate()?;
        let mut sync = SyncManager::new(Box::new(bluetooth::LinuxBluetoothManager::new()?), efi_context);
        return if args[0] == "--audit-sync" { sync.preview_bidirectional() } else { sync.sync_bidirectional() };
    }

    // Create tokio runtime and run async code
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_service())
}

async fn run_service() -> Result<(), Box<dyn Error>> {
    log!("[BlueVein] Reconciling the Bluetooth service before startup synchronization...");
    bluetooth::LinuxBluetoothManager::ensure_bluetooth_running()?;
    let bt_manager = Box::new(bluetooth::LinuxBluetoothManager::new()?);

    let efi_context = EfiContext::from_env();
    efi_context.validate()?;

    let mut sync_manager = SyncManager::new(bt_manager, efi_context);

    log!("[BlueVein] Performing initial bidirectional sync...");
    // Use bidirectional sync to properly merge EFI and system state.
    // A bond that refused to unpair is retried by the next cycle; every other
    // startup failure still stops the service before it monitors exports.
    if let Err(e) = sync_manager.sync_bidirectional() {
        if e.downcast_ref::<crate::sync::UnfinishedRemovals>().is_none() { return Err(e); }
        log!("[BlueVein] Continuing after {}", e);
    }

    // Start monitoring Bluetooth changes
    log!("[BlueVein] Starting Bluetooth monitoring...");
    monitor::monitor_bluetooth_changes(sync_manager).await
}

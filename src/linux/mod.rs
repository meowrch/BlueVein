mod bluetooth;
mod monitor;

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

    // Create tokio runtime and run async code
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_service())
}

async fn run_service() -> Result<(), Box<dyn Error>> {
    let bt_manager = Box::new(bluetooth::LinuxBluetoothManager::new()?);
    let mut sync_manager = SyncManager::new(bt_manager);

    log!("[BlueVein] Performing initial bidirectional sync...");
    // Use bidirectional sync to properly merge EFI and system state
    if let Err(e) = sync_manager.sync_bidirectional() {
        log!("[BlueVein] Warning: Initial sync failed: {}", e);
    }

    // Start monitoring Bluetooth changes
    log!("[BlueVein] Starting Bluetooth monitoring...");
    monitor::monitor_bluetooth_changes(sync_manager).await
}

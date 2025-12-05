mod bluetooth;
mod monitor;
mod service;

use crate::log;
use crate::sync::SyncManager;
use std::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

pub fn run() -> Result<(), Box<dyn Error>> {
    // Check if running as service or standalone
    if std::env::args().any(|arg| arg == "--service") {
        service::run_service()
    } else {
        // Parse command line arguments
        let args: Vec<String> = std::env::args().collect();

        if args.len() > 1 {
            match args[1].as_str() {
                "install" => service::install_service(),
                "uninstall" => service::uninstall_service(),
                "start" => service::start_service(),
                "stop" => service::stop_service(),
                _ => {
                    log!("BlueVein - Bluetooth Synchronization Service");
                    log!("\nUsage:");
                    log!("  bluevein.exe install   - Install service");
                    log!("  bluevein.exe uninstall - Uninstall service");
                    log!("  bluevein.exe start     - Start service");
                    log!("  bluevein.exe stop      - Stop service");
                    Ok(())
                }
            }
        } else {
            // Run standalone (for testing)
            log!("[BlueVein] Running in standalone mode...");
            run_sync_loop()
        }
    }
}

pub fn run_sync_loop() -> Result<(), Box<dyn Error>> {
    let bt_manager = Box::new(bluetooth::WindowsBluetoothManager::new()?);
    let mut sync_manager = SyncManager::new(bt_manager);

    log!("[BlueVein] Performing initial bidirectional sync...");
    if let Err(e) = sync_manager.sync_bidirectional() {
        log!("[BlueVein] Warning: Initial sync failed: {}", e);
        log!("[BlueVein] Continuing with monitoring...");
    }

    let running = Arc::new(AtomicBool::new(true));

    // Set up Ctrl+C handler for standalone mode
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        log!("\n[BlueVein] Shutting down...");
        running_clone.store(false, Ordering::Relaxed);
    })
    .ok();

    // Start periodic EFI checker in background thread
    let running_efi = running.clone();
    thread::spawn(move || {
        periodic_efi_check(running_efi);
    });

    // Start monitoring with registry change notifications
    log!("[BlueVein] Starting registry monitoring...");
    monitor::monitor_bluetooth_changes(sync_manager, running)
}

/// Periodically check EFI for changes made by other OS
fn periodic_efi_check(running: Arc<AtomicBool>) {
    let bt_manager = match bluetooth::WindowsBluetoothManager::new() {
        Ok(mgr) => mgr,
        Err(e) => {
            log!(
                "[BlueVein] Failed to create BT manager for EFI checking: {}",
                e
            );
            return;
        }
    };

    let mut sync_manager = SyncManager::new(Box::new(bt_manager));

    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(30)); // Check every 30 seconds

        if !running.load(Ordering::Relaxed) {
            break;
        }

        if let Err(e) = sync_manager.check_efi_changes() {
            log!("[BlueVein] Error checking EFI changes: {}", e);
        }
    }
}

mod bluetooth;
mod monitor;
mod service;

use crate::efi::EfiContext;
use crate::log;
use crate::sync::SyncManager;
use std::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub fn run() -> Result<(), Box<dyn Error>> {
    // Check if running as service or standalone
    if std::env::args().any(|arg| arg == "--service") {
        service::run_service()
    } else {
        // Parse command line arguments
        let args: Vec<String> = std::env::args().collect();

        if args.len() > 1 {
            match args[1].as_str() {
                "audit-sync" | "sync-once" | "repair-efi-only" => {
                    if args.len() != 2 && args.len() != 4 {
                        return Err("Usage: audit-sync|sync-once [adapter-mac identity-mac]".into());
                    }
                    if args.len() == 4 {
                        for address in &args[2..4] {
                            if !crate::bluetooth::is_valid_mac_hex(&crate::bluetooth::mac_to_windows_format(address)) {
                                return Err("Invalid scoped Bluetooth address".into());
                            }
                        }
                        std::env::set_var("BLUEVEIN_ADAPTER_FILTER", &args[2]);
                        std::env::set_var("BLUEVEIN_DEVICE_FILTER", &args[3]);
                    }
                    let manager = Box::new(bluetooth::WindowsBluetoothManager::new()?);
                    let context = EfiContext::from_env();
                    let mut sync = SyncManager::new(manager, context);
                    match args[1].as_str() {
                        "audit-sync" => sync.preview_bidirectional(),
                        "repair-efi-only" => sync.repair_efi_only(),
                        _ => sync.sync_bidirectional(),
                    }
                },
                "install" => service::install_service(),
                "uninstall" => service::uninstall_service(),
                "start" => service::start_service(),
                "stop" => service::stop_service(),
                _ => {
                    log!("BlueVein - Bluetooth Synchronization Service");
                    log!("\nUsage:");
                    log!("  bluevein.exe audit-sync - Preview synchronization without changing keys");
                    log!("  bluevein.exe sync-once  - Synchronize once, then exit");
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

    let efi_context = EfiContext::from_env();
    efi_context.validate()?;

    let mut sync_manager = SyncManager::new(bt_manager, efi_context);

    log!("[BlueVein] Performing initial bidirectional sync...");
    // A bond that refused to unpair keeps its marker and is retried by the next
    // cycle; every other startup failure still stops before monitoring.
    if let Err(e) = sync_manager.sync_bidirectional() {
        if e.downcast_ref::<crate::sync::UnfinishedRemovals>().is_none() { return Err(e); }
        log!("[BlueVein] Continuing after {}", e);
    }

    let running = Arc::new(AtomicBool::new(true));

    // Set up Ctrl+C handler for standalone mode
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        log!("\n[BlueVein] Shutting down...");
        running_clone.store(false, Ordering::Relaxed);
    })
    .ok();

    // Start monitoring with registry change notifications
    log!("[BlueVein] Starting registry monitoring...");
    monitor::monitor_bluetooth_changes(sync_manager, running)
}

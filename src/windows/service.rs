use crate::log;
use std::error::Error;
use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

const SERVICE_NAME: &str = "BlueVeinService";
const SERVICE_DISPLAY_NAME: &str = "BlueVein Bluetooth Sync";
const SERVICE_DESCRIPTION: &str = "Synchronizes Bluetooth devices between Windows and Linux";

define_windows_service!(ffi_service_main, service_main);

pub fn run_service() -> Result<(), Box<dyn Error>> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| format!("Service dispatcher failed: {}", e).into())
}

fn service_main(arguments: Vec<OsString>) {
    if let Err(e) = run_service_impl(arguments) {
        log!("Service error: {}", e);
    }
}

fn run_service_impl(_arguments: Vec<OsString>) -> Result<(), Box<dyn Error>> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Interrogate => {
                shutdown_tx.send(()).ok();
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // Tell Windows we're starting
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Run the actual service logic
    let service_thread = std::thread::spawn(move || {
        if let Err(e) = super::run_sync_loop() {
            log!("[BlueVein] Service error: {}", e);
        }
    });

    // Wait for shutdown signal
    let _ = shutdown_rx.recv();

    // Tell Windows we're stopping
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    drop(service_thread);
    Ok(())
}

pub fn install_service() -> Result<(), Box<dyn Error>> {
    use std::env;
    use std::process::Command;

    let exe_path = env::current_exe()?;
    let exe_path_str = exe_path.to_str().ok_or("Invalid executable path")?;

    // Use sc.exe to install service
    // IMPORTANT: No spaces after = signs!
    let output = Command::new("sc.exe")
        .args(&[
            "create",
            SERVICE_NAME,
            &format!("binPath=\"{}\" --service", exe_path_str),
            &format!("DisplayName={}", SERVICE_DISPLAY_NAME),
            "start=auto",
            "depend=BthServ",
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "Failed to install service:\nstdout: {}\nstderr: {}",
            stdout, stderr
        )
        .into());
    }

    // Set description
    let desc_output = Command::new("sc.exe")
        .args(&["description", SERVICE_NAME, SERVICE_DESCRIPTION])
        .output()?;

    if !desc_output.status.success() {
        log!("[BlueVein] Warning: Failed to set service description");
    }

    log!("[BlueVein] Service installed successfully!");
    log!("[BlueVein] Use 'bluevein.exe start' to start the service");
    Ok(())
}

pub fn uninstall_service() -> Result<(), Box<dyn Error>> {
    use std::process::Command;

    // Try to stop first
    let _ = Command::new("sc.exe")
        .args(&["stop", SERVICE_NAME])
        .output();

    // Wait a bit for service to stop
    std::thread::sleep(std::time::Duration::from_secs(1));

    let output = Command::new("sc.exe")
        .args(&["delete", SERVICE_NAME])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "Failed to uninstall service:\nstdout: {}\nstderr: {}",
            stdout, stderr
        )
        .into());
    }

    log!("[BlueVein] Service uninstalled successfully!");
    Ok(())
}

pub fn start_service() -> Result<(), Box<dyn Error>> {
    use std::process::Command;

    let output = Command::new("sc.exe")
        .args(&["start", SERVICE_NAME])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "Failed to start service:\nstdout: {}\nstderr: {}",
            stdout, stderr
        )
        .into());
    }

    log!("[BlueVein] Service started successfully!");
    Ok(())
}

pub fn stop_service() -> Result<(), Box<dyn Error>> {
    use std::process::Command;

    let output = Command::new("sc.exe")
        .args(&["stop", SERVICE_NAME])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "Failed to stop service:\nstdout: {}\nstderr: {}",
            stdout, stderr
        )
        .into());
    }

    log!("[BlueVein] Service stopped successfully!");
    Ok(())
}

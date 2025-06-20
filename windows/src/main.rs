use anyhow::Result;
use shared::{Config, get_adapter_mac, update_common_file};
use windows_service::{
    service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType},
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};
use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Registry::{RegNotifyChangeKeyValue, HKEY, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_FILTER};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use log::{info, error};
use std::time::Duration;

const SERVICE_NAME: &str = "bluevein-windows";
const CONFIG_PATH: &str = "C:\\ProgramData\\bluevein-windows\\config.json";

fn main() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

extern "system" fn ffi_service_main(_argc: u32, _argv: *mut *mut u16) {
    if let Err(e) = run_service() {
        error!("Service error: {}", e);
    }
}

fn run_service() -> Result<()> {
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    main_loop()?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

fn main_loop() -> Result<()> {
    env_logger::init();

    let mut config = Config::load(CONFIG_PATH)?;

    let adapter_mac = if let Some(mac) = &config.adapter_mac {
        mac.clone()
    } else {
        let mac = get_adapter_mac()?;
        config.adapter_mac = Some(mac.clone());
        config.save(CONFIG_PATH)?;
        mac
    };

    info!("Using Bluetooth adapter: {}", adapter_mac);

    sync_keys_to_file(&config.efi_path, &adapter_mac)?;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let reg_path = format!("SYSTEM\\CurrentControlSet\\Services\\BTHPORT\\Parameters\\Keys\\{}", adapter_mac);
    let key = hklm.open_subkey(&reg_path)?;

    unsafe {
        let event = CreateEventW(None, true, false, None)?;
        loop {
            RegNotifyChangeKeyValue(
                HKEY(key.raw_handle() as isize),
                BOOL(0),
                REG_NOTIFY_FILTER(REG_NOTIFY_CHANGE_LAST_SET.0 as u32),
                event,
                BOOL(1),
            )?;

            WaitForSingleObject(event, INFINITE);

            if let Err(e) = sync_keys_to_file(&config.efi_path, &adapter_mac) {
                error!("Sync failed: {}", e);
            }
        }
    }
}

fn sync_keys_to_file(efi_path: &str, adapter_mac: &str) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::*;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let reg_path = format!("SYSTEM\\CurrentControlSet\\Services\\BTHPORT\\Parameters\\Keys\\{}", adapter_mac);
    let key = hklm.open_subkey(&reg_path)?;

    for value_name in key.enum_values().filter_map(|x| x.ok().map(|(name, _)| name.to_string())) {
        if let Ok(value) = key.get_raw_value(&value_name) {
            if value.bytes.len() == 16 {
                let key_hex = hex::encode(&value.bytes);
                update_common_file(efi_path, adapter_mac, &value_name, &key_hex)?;
            }
        }
    }

    Ok(())
}

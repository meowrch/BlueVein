use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub efi_path: String,
    pub adapter_mac: Option<String>,
}

impl Config {
    pub fn load(config_path: &str) -> Result<Self> {
        if Path::new(config_path).exists() {
            let content = fs::read_to_string(config_path)
                .context("Failed to read config file")?;
            serde_json::from_str(&content)
                .context("Failed to parse config file")
        } else {
            let config = Self::default();
            config.save(config_path)?;
            Ok(config)
        }
    }

    pub fn save(&self, config_path: &str) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        if let Some(parent) = Path::new(config_path).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(config_path, content)?;
        Ok(())
    }

    pub fn default() -> Self {
        Self {
            efi_path: Self::find_efi().unwrap_or_else(|| {
                #[cfg(target_os = "linux")]
                { "/boot/efi".to_string() }
                #[cfg(windows)]
                { "C:\\EFI".to_string() }
            }),
            adapter_mac: None,
        }
    }

    pub fn find_efi() -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            for path in &["/boot/efi", "/boot", "/efi"] {
                if Path::new(path).exists() {
                    return Some(path.to_string());
                }
            }
        }

        #[cfg(windows)]
        {
            for drive_letter in ('A'..='Z').map(|c| format!("{}:", c)) {
                let path = format!("{}\\EFI", drive_letter);
                if Path::new(&path).exists() {
                    return Some(drive_letter);
                }
            }
        }

        None
    }

    pub fn keys_path(&self) -> String {
        format!("{}/bt_keys.json", self.efi_path)
    }
}

pub fn update_common_file(efi_path: &str, adapter_mac: &str, device_mac: &str, key: &str) -> Result<()> {
    let file_path = format!("{}/bt_keys.json", efi_path);
    let mut root: serde_json::Value = if Path::new(&file_path).exists() {
        let content = fs::read_to_string(&file_path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::json!({ "adapter_mac": adapter_mac, "devices": {} })
    };

    if let Some(devices) = root["devices"].as_object_mut() {
        devices.insert(device_mac.to_uppercase(), serde_json::Value::String(key.to_string()));
    } else {
        let mut devices = serde_json::Map::new();
        devices.insert(device_mac.to_uppercase(), serde_json::Value::String(key.to_string()));
        root["devices"] = serde_json::Value::Object(devices);
    }

    root["adapter_mac"] = serde_json::Value::String(adapter_mac.to_string());
    
    let content = serde_json::to_string_pretty(&root)?;
    fs::write(file_path, content)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn get_adapter_mac() -> Result<String> {
    use dbus::{blocking::Connection, arg};
    use std::time::Duration;

    let conn = Connection::new_system()?;
    let proxy = conn.with_proxy("org.bluez", "/", Duration::from_secs(5));
    
    let (objects,): (std::collections::HashMap<dbus::Path, std::collections::HashMap<String, std::collections::HashMap<String, arg::Variant<Box<dyn arg::RefArg>>>>>,) = 
        proxy.method_call("org.freedesktop.DBus.ObjectManager", "GetManagedObjects", ())?;

    for (_, interfaces) in objects {
        if let Some(adapter_props) = interfaces.get("org.bluez.Adapter1") {
            if let Some(addr) = adapter_props.get("Address").and_then(|v| v.0.as_str()) {
                return Ok(addr.to_string());
            }
        }
    }
    
    anyhow::bail!("Bluetooth adapter not found");
}

#[cfg(windows)]
pub fn get_adapter_mac() -> Result<String> {
    use winreg::{RegKey, enums::*};
    
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let keys_path = "SYSTEM\\CurrentControlSet\\Services\\BTHPORT\\Parameters\\Keys";
    
    match hklm.open_subkey(keys_path) {
        Ok(keys) => {
            for subkey_name in keys.enum_keys().filter_map(|x| x.ok()) {
                // Возвращаем первый найденный MAC адаптера
                return Ok(subkey_name);
            }
            anyhow::bail!("No Bluetooth adapters found in registry")
        },
        Err(e) => {
            anyhow::bail!("Failed to open registry key: {}", e)
        }
    }
}

#[cfg(target_os = "linux")]
pub fn read_link_key(adapter_mac: &str, device_mac: &str) -> Option<String> {
    let path = format!("/var/lib/bluetooth/{}/{}/info", adapter_mac, device_mac);

    
    if let Ok(data) = fs::read_to_string(path) {
        for line in data.lines() {
            if line.starts_with("Key=") {
                return Some(line[4..].to_string());
            }
        }
    }
    None
}
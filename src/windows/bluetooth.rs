use crate::bluetooth::{
    mac_to_windows_format, normalize_mac, validate_bluetooth_key, windows_format_to_mac,
    BluetoothDevice, BluetoothManager, ClassicKeys, CsrkKey, LeLongTermKey, LeKeys,
};
use crate::log;
use std::error::Error;
use winreg::enums::*;
use winreg::enums::RegDisposition;
use winreg::RegKey;

const BLUETOOTH_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Keys";
const BLUETOOTH_LE_REG_PATH: &str =
    r"SYSTEM\CurrentControlSet\Services\BTHLE\Parameters\Keys";

pub struct WindowsBluetoothManager {
    hklm: RegKey,
}

impl WindowsBluetoothManager {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        Ok(Self { hklm })
    }

    fn open_bluetooth_keys(&self) -> Result<RegKey, Box<dyn Error>> {
        self.hklm
            .open_subkey_with_flags(BLUETOOTH_REG_PATH, KEY_READ | KEY_WRITE)
            .map_err(|e| {
                format!(
                    "Failed to open Bluetooth registry key (need admin rights): {}",
                    e
                )
                .into()
            })
    }

    fn open_bluetooth_le_keys(&self) -> Result<RegKey, Box<dyn Error>> {
        self.hklm
            .open_subkey_with_flags(BLUETOOTH_LE_REG_PATH, KEY_READ | KEY_WRITE)
            .map_err(|e| format!("Failed to open Bluetooth LE registry key: {}", e).into())
    }

    /// Ensure Bluetooth LE registry path exists
    /// Creates the base BTHLE\Parameters\Keys path if missing
    fn ensure_bluetooth_le_keys(&self) -> Result<RegKey, Box<dyn Error>> {
        match self.open_bluetooth_le_keys() {
            Ok(keys) => Ok(keys),
            Err(_) => {
                // Try to create the path if it doesn't exist
                log!("[BlueVein] LE registry path doesn't exist, attempting to create: {}", BLUETOOTH_LE_REG_PATH);
                
                self.hklm
                    .create_subkey(BLUETOOTH_LE_REG_PATH)
                    .map(|(key, _)| key)
                    .map_err(|e| {
                        format!(
                            "Failed to create Bluetooth LE registry path (need admin rights): {}",
                            e
                        )
                        .into()
                    })
            }
        }
    }

    /// Read classic Bluetooth device keys
    fn read_classic_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<Option<ClassicKeys>, Box<dyn Error>> {
        let bt_keys = match self.open_bluetooth_keys() {
            Ok(keys) => keys,
            Err(_) => return Ok(None),
        };

        let adapter_key_name = mac_to_windows_format(adapter_mac);
        let device_key_name = mac_to_windows_format(device_mac);

        let adapter_key = match bt_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ) {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };

        // Read raw value as binary
        if let Ok(value) = adapter_key.get_raw_value(&device_key_name) {
            let link_key = hex::encode(&value.bytes).to_uppercase();

            // Validate LinkKey length
            if let Err(e) = validate_bluetooth_key(&link_key, "LinkKey") {
                log!(
                    "[BlueVein] Warning: Invalid LinkKey for device {}: {}",
                    device_mac,
                    e
                );
                return Ok(None);
            }

            return Ok(Some(ClassicKeys::new(link_key)));
        }

        Ok(None)
    }

    /// Read LE device keys
    fn read_le_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<Option<LeKeys>, Box<dyn Error>> {
        let bt_le_keys = match self.open_bluetooth_le_keys() {
            Ok(keys) => keys,
            Err(_) => return Ok(None),
        };

        let adapter_key_name = mac_to_windows_format(adapter_mac);
        let device_key_name = mac_to_windows_format(device_mac);

        let adapter_key = match bt_le_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ) {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };

        let device_key = match adapter_key.open_subkey_with_flags(&device_key_name, KEY_READ) {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };

        let mut le_keys = LeKeys::default();
        let mut has_keys = false;

        // Read LTK (Long Term Key)
        if let Ok(ltk_value) = device_key.get_raw_value("LTK") {
            let key = hex::encode(&ltk_value.bytes).to_uppercase();

            // Validate LTK length
            if let Err(e) = validate_bluetooth_key(&key, "LTK") {
                log!(
                    "[BlueVein] Warning: Invalid LTK for device {}: {}",
                    device_mac,
                    e
                );
            } else {
                let authenticated = device_key
                    .get_value::<u32, _>("Authenticated")
                    .ok()
                    .map(|v| v as u8);
                let enc_size = device_key
                    .get_value::<u32, _>("KeyLength")
                    .ok()
                    .map(|v| v as u8);
                let ediv = device_key.get_value::<u32, _>("EDIV").ok().map(|v| v as u16);
                let rand = device_key.get_value::<u64, _>("ERand").ok();

                le_keys.ltk = Some(LeLongTermKey {
                    key,
                    authenticated,
                    enc_size,
                    ediv,
                    rand,
                });
                has_keys = true;
            }
        }

        // Read IRK (Identity Resolving Key)
        if let Ok(irk_value) = device_key.get_raw_value("IRK") {
            let key = hex::encode(&irk_value.bytes).to_uppercase();

            // Validate IRK length
            if let Err(e) = validate_bluetooth_key(&key, "IRK") {
                log!(
                    "[BlueVein] Warning: Invalid IRK for device {}: {}",
                    device_mac,
                    e
                );
            } else {
                le_keys.irk = Some(key);
                has_keys = true;
            }
        }

        // Read CSRK (Connection Signature Resolving Key)
        //
        // WINDOWS LIMITATION: SignCounter not persisted in registry
        // =========================================================
        // Per Bluetooth Core Spec v5.3, SignCounter MUST increment with each signed packet
        // to prevent replay attacks. However, Windows Bluetooth stack does NOT store this
        // counter in registry - it's kept only in volatile driver memory.
        //
        // BLUEVEIN SOLUTION: Smart Counter synchronization
        // =================================================
        // 1. Counter is stored in bluevein.json (persisted across reboots)
        // 2. During device merge (see merge_devices in sync.rs), we take MAX counter value
        // 3. This prevents counter rollback and maintains replay attack protection
        //
        // IMPACT: Minimal for modern devices
        // ===================================
        // Most LE devices (keyboards, mice, headphones, gamepads) use LTK for encrypted
        // connections. CSRK signing is only used by rare IoT devices with unencrypted
        // connections. If such device fails to connect after sync, re-pair once to reset.
        if let Ok(csrk_value) = device_key.get_raw_value("CSRK") {
            let key = hex::encode(&csrk_value.bytes).to_uppercase();

            // Validate CSRK length
            if let Err(e) = validate_bluetooth_key(&key, "CSRK (Local)") {
                log!(
                    "[BlueVein] Warning: Invalid CSRK for device {}: {}",
                    device_mac,
                    e
                );
            } else {
                // Windows doesn't store Counter/Authenticated in registry, use defaults
                le_keys.csrk_local = Some(CsrkKey::new(key));
                has_keys = true;
            }
        }

        // Read CSRKInbound (Remote CSRK)
        // Same Counter limitation applies to remote CSRK
        if let Ok(csrk_inbound) = device_key.get_raw_value("CSRKInbound") {
            let key = hex::encode(&csrk_inbound.bytes).to_uppercase();

            // Validate CSRK length
            if let Err(e) = validate_bluetooth_key(&key, "CSRK (Remote)") {
                log!(
                    "[BlueVein] Warning: Invalid CSRKInbound for device {}: {}",
                    device_mac,
                    e
                );
            } else {
                le_keys.csrk_remote = Some(CsrkKey::new(key));
                has_keys = true;
            }
        }

        if has_keys {
            Ok(Some(le_keys))
        } else {
            Ok(None)
        }
    }

    /// Write classic device keys
    fn write_classic_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
        classic: &ClassicKeys,
    ) -> Result<(), Box<dyn Error>> {
        // Validate LinkKey before writing
        validate_bluetooth_key(&classic.link_key, "LinkKey")?;

        let bt_keys = self.open_bluetooth_keys()?;
        let adapter_key_name = mac_to_windows_format(adapter_mac);
        let device_key_name = mac_to_windows_format(device_mac);

        // Open or create adapter key
        let (adapter_key, _) = bt_keys.create_subkey(&adapter_key_name).map_err(|e| {
            format!(
                "Failed to create/open adapter key {}: {}",
                adapter_key_name, e
            )
        })?;

        // Decode hex link key to bytes
        let key_bytes = hex::decode(&classic.link_key)
            .map_err(|e| format!("Invalid link key format: {}", e))?;

        // Write as binary value (REG_BINARY)
        adapter_key
            .set_raw_value(
                &device_key_name,
                &winreg::RegValue {
                    bytes: key_bytes,
                    vtype: winreg::enums::RegType::REG_BINARY,
                },
            )
            .map_err(|e| format!("Failed to write device key: {}", e))?;

        Ok(())
    }

    /// Write LE device keys
    /// 
    /// This function ensures the full registry path exists and creates it if needed.
    /// Windows only creates BTHLE registry entries when devices connect via LE,
    /// so we need to create the structure manually when syncing from another OS.
    fn write_le_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
        le: &LeKeys,
    ) -> Result<(), Box<dyn Error>> {
        // Ensure base LE registry path exists (create if needed)
        let bt_le_keys = self.ensure_bluetooth_le_keys()?;
        
        let adapter_key_name = mac_to_windows_format(adapter_mac);
        let device_key_name = mac_to_windows_format(device_mac);

        // Create adapter key if it doesn't exist
        let (adapter_key, adapter_disp) = bt_le_keys
            .create_subkey(&adapter_key_name)
            .map_err(|e| {
                format!(
                    "Failed to create adapter key {} in LE registry: {}",
                    adapter_key_name, e
                )
            })?;
        
        if adapter_disp == RegDisposition::REG_CREATED_NEW_KEY {
            log!("[BlueVein]   Created new adapter key in LE registry: {}", adapter_key_name);
        }

        // Create device key - this is where LE keys are stored
        let (device_key, device_disp) = adapter_key
            .create_subkey(&device_key_name)
            .map_err(|e| {
                format!(
                    "Failed to create device key {} in LE registry: {}",
                    device_key_name, e
                )
            })?;
        
        if device_disp == RegDisposition::REG_CREATED_NEW_KEY {
            log!("[BlueVein]   Created new device key in LE registry: {}", device_key_name);
        } else {
            log!("[BlueVein]   Updating existing device key in LE registry: {}", device_key_name);
        }

        // Write LTK
        if let Some(ltk) = &le.ltk {
            // Validate LTK before writing
            validate_bluetooth_key(&ltk.key, "LTK")?;

            let ltk_bytes =
                hex::decode(&ltk.key).map_err(|e| format!("Invalid LTK format: {}", e))?;

            device_key.set_raw_value(
                "LTK",
                &winreg::RegValue {
                    bytes: ltk_bytes,
                    vtype: RegType::REG_BINARY,
                },
            )?;

            // Use authenticated_or_default() to ensure default value of 0
            device_key.set_value("Authenticated", &(ltk.authenticated_or_default() as u32))?;

            if let Some(enc_size) = ltk.enc_size {
                device_key.set_value("KeyLength", &(enc_size as u32))?;
            }
            if let Some(ediv) = ltk.ediv {
                device_key.set_value("EDIV", &(ediv as u32))?;
            }
            if let Some(rand) = ltk.rand {
                device_key.set_value("ERand", &rand)?;
            }
        }

        // Write IRK
        if let Some(irk) = &le.irk {
            // Validate IRK before writing
            validate_bluetooth_key(irk, "IRK")?;

            let irk_bytes = hex::decode(irk).map_err(|e| format!("Invalid IRK format: {}", e))?;

            device_key.set_raw_value(
                "IRK",
                &winreg::RegValue {
                    bytes: irk_bytes,
                    vtype: RegType::REG_BINARY,
                },
            )?;
        }

        // Write CSRK (local)
        //
        // NOTE: Windows registry does NOT store Counter/Authenticated fields
        // ===================================================================
        // BlueVein manages these fields in bluevein.json for proper synchronization.
        // The merge_devices() function in sync.rs handles smart Counter merging:
        // - Takes MAX counter value when keys match (prevents rollback)
        // - Combines authenticated flags with OR logic
        // This ensures replay attack protection even without registry support.
        if let Some(csrk_local) = &le.csrk_local {
            // Validate CSRK before writing
            validate_bluetooth_key(&csrk_local.key, "CSRK (Local)")?;

            let csrk_bytes = hex::decode(&csrk_local.key)
                .map_err(|e| format!("Invalid CSRK format: {}", e))?;

            device_key.set_raw_value(
                "CSRK",
                &winreg::RegValue {
                    bytes: csrk_bytes,
                    vtype: RegType::REG_BINARY,
                },
            )?;
        }

        // Write CSRKInbound (remote)
        // Same Counter/Authenticated limitation and BlueVein solution as local CSRK
        if let Some(csrk_remote) = &le.csrk_remote {
            // Validate CSRK before writing
            validate_bluetooth_key(&csrk_remote.key, "CSRK (Remote)")?;

            let csrk_bytes = hex::decode(&csrk_remote.key)
                .map_err(|e| format!("Invalid CSRKInbound format: {}", e))?;

            device_key.set_raw_value(
                "CSRKInbound",
                &winreg::RegValue {
                    bytes: csrk_bytes,
                    vtype: RegType::REG_BINARY,
                },
            )?;
        }

        Ok(())
    }
}

impl BluetoothManager for WindowsBluetoothManager {
    fn get_adapters(&self) -> Result<Vec<String>, Box<dyn Error>> {
        let mut adapters = Vec::new();

        // Check classic Bluetooth adapters
        if let Ok(bt_keys) = self.open_bluetooth_keys() {
            for adapter in bt_keys.enum_keys() {
                if let Ok(adapter_name) = adapter {
                    let mac = windows_format_to_mac(&adapter_name);
                    if !adapters.contains(&mac) {
                        adapters.push(mac);
                    }
                }
            }
        }

        // Check LE adapters
        if let Ok(bt_le_keys) = self.open_bluetooth_le_keys() {
            for adapter in bt_le_keys.enum_keys() {
                if let Ok(adapter_name) = adapter {
                    let mac = windows_format_to_mac(&adapter_name);
                    if !adapters.contains(&mac) {
                        adapters.push(mac);
                    }
                }
            }
        }

        Ok(adapters)
    }

    fn get_devices(&self, adapter_mac: &str) -> Result<Vec<BluetoothDevice>, Box<dyn Error>> {
        let mut devices_map: std::collections::HashMap<String, BluetoothDevice> =
            std::collections::HashMap::new();

        // Read classic devices
        if let Ok(bt_keys) = self.open_bluetooth_keys() {
            let adapter_key_name = mac_to_windows_format(adapter_mac);
            if let Ok(adapter_key) = bt_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ) {
                for device in adapter_key.enum_values() {
                    if let Ok((device_name, _)) = device {
                        let device_mac = windows_format_to_mac(&device_name);
                        if let Ok(Some(classic)) = self.read_classic_device(adapter_mac, &device_mac)
                        {
                            devices_map
                                .entry(device_mac.clone())
                                .or_insert_with(|| BluetoothDevice {
                                    mac_address: device_mac.clone(),
                                    classic: None,
                                    le: None,
                                })
                                .classic = Some(classic);
                        }
                    }
                }
            }
        }

        // Read LE devices
        if let Ok(bt_le_keys) = self.open_bluetooth_le_keys() {
            let adapter_key_name = mac_to_windows_format(adapter_mac);
            if let Ok(adapter_key) = bt_le_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ)
            {
                for device in adapter_key.enum_keys() {
                    if let Ok(device_name) = device {
                        let device_mac = windows_format_to_mac(&device_name);
                        if let Ok(Some(le)) = self.read_le_device(adapter_mac, &device_mac) {
                            devices_map
                                .entry(device_mac.clone())
                                .or_insert_with(|| BluetoothDevice {
                                    mac_address: device_mac.clone(),
                                    classic: None,
                                    le: None,
                                })
                                .le = Some(le);
                        }
                    }
                }
            }
        }

        Ok(devices_map.into_iter().map(|(_, device)| device).collect())
    }

    fn get_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<BluetoothDevice, Box<dyn Error>> {
        let classic = self.read_classic_device(adapter_mac, device_mac)?;
        let le = self.read_le_device(adapter_mac, device_mac)?;

        if classic.is_none() && le.is_none() {
            return Err(format!("Device {} not found", device_mac).into());
        }

        Ok(BluetoothDevice {
            mac_address: normalize_mac(device_mac),
            classic,
            le,
        })
    }

    fn set_device(
        &mut self,
        adapter_mac: &str,
        device: &BluetoothDevice,
    ) -> Result<(), Box<dyn Error>> {
        // Write classic keys if present
        if let Some(classic) = &device.classic {
            self.write_classic_device(adapter_mac, &device.mac_address, classic)?;
        }

        // Write LE keys if present
        if let Some(le) = &device.le {
            self.write_le_device(adapter_mac, &device.mac_address, le)?;
        }

        Ok(())
    }

    fn remove_device(
        &mut self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<(), Box<dyn Error>> {
        let adapter_key_name = mac_to_windows_format(adapter_mac);
        let device_key_name = mac_to_windows_format(device_mac);

        // Remove from classic registry
        if let Ok(bt_keys) = self.open_bluetooth_keys() {
            if let Ok(adapter_key) = bt_keys.open_subkey_with_flags(&adapter_key_name, KEY_WRITE) {
                let _ = adapter_key.delete_value(&device_key_name);
            }
        }

        // Remove from LE registry
        if let Ok(bt_le_keys) = self.open_bluetooth_le_keys() {
            if let Ok(adapter_key) =
                bt_le_keys.open_subkey_with_flags(&adapter_key_name, KEY_WRITE)
            {
                let _ = adapter_key.delete_subkey(&device_key_name);
            }
        }

        Ok(())
    }
}

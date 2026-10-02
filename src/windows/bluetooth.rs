use crate::bluetooth::{
    is_valid_mac_hex, mac_to_windows_format, normalize_mac, validate_bluetooth_key,
    windows_format_to_mac, BluetoothDevice, BluetoothManager, ClassicKeys, CsrkKey,
    LeLongTermKey, LeKeys,
};
use crate::log;
use std::error::Error;
use winreg::enums::RegDisposition;
use winreg::enums::*;
use winreg::RegKey;

const BLUETOOTH_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Keys";
const BLUETOOTH_LE_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Keys";
const BLUETOOTH_DEVICE_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Devices";
/// PnP enumerators that hold the friendly name Windows itself displays.
/// BTHLE first: its records belong to one peer, while BTHENUM also lists
/// per-service nodes whose names describe a profile rather than the device.
const PNP_ENUM_PATHS: [&str; 2] = [
    r"SYSTEM\CurrentControlSet\Enum\BTHLE",
    r"SYSTEM\CurrentControlSet\Enum\BTHENUM",
];
/// ERROR_NOT_FOUND: the Bluetooth stack has no device object for this address.
const ERROR_NOT_FOUND: u32 = 1168;

// Read only: enumerate present LE devnodes; never scan or connect a radio.
#[link(name = "cfgmgr32")]
extern "system" {
    fn CM_Get_Device_ID_List_SizeW(len: *mut u32, filter: *const u16, flags: u32) -> u32;
    fn CM_Get_Device_ID_ListW(filter: *const u16, buffer: *mut u16, len: u32, flags: u32) -> u32;
}

#[cfg(test)]
thread_local! { static TEST_PRESENT_LE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) }; }

fn present_le_storage_names() -> Result<Vec<String>, Box<dyn Error>> {
    #[cfg(test)]
    if let Some(names) = TEST_PRESENT_LE.with(|v| v.borrow().clone()) { return Ok(names); }

    let filter: Vec<u16> = "BTHLE\0".encode_utf16().collect();
    // CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT.
    let flags = 0x00000101;
    for _ in 0..3 {
        let mut len = 0u32;
        if unsafe { CM_Get_Device_ID_List_SizeW(&mut len, filter.as_ptr(), flags) } != 0 {
            return Err("Cannot enumerate present Bluetooth LE devices".into());
        }
        if len == 0 || len > 1_048_576 { return Err("Invalid Bluetooth device list size".into()); }
        let mut buffer = vec![0u16; len as usize];
        if unsafe { CM_Get_Device_ID_ListW(filter.as_ptr(), buffer.as_mut_ptr(), len, flags) } != 0 {
            continue; // Device tree may change between size and list calls.
        }
        return Ok(buffer.split(|c| *c == 0).filter_map(|part| {
            let id = String::from_utf16(part).ok()?.to_ascii_uppercase();
            let mut parts = id.split('\\');
            if parts.next()? != "BTHLE" { return None; }
            let mac = parts.next()?.strip_prefix("DEV_")?;
            if !is_valid_mac_hex(mac) || parts.next().is_none() { return None; }
            Some(mac.to_string())
        }).collect());
    }
    Err("Bluetooth device list changed during enumeration".into())
}

fn unique_present_storage(candidates: &[String], present: &[String]) -> Option<String> {
    let mut matches = candidates.iter().filter(|name| present.iter().any(|p| p.eq_ignore_ascii_case(name)));
    let first = matches.next()?.clone();
    if matches.next().is_some() { None } else { Some(first) }
}

pub struct WindowsBluetoothManager {
    hklm: RegKey,
}

impl WindowsBluetoothManager {
    /// Device display names are separate from BTHPORT pairing keys. A missing
    /// cache entry must never prevent key synchronization.
    fn read_device_name(&self, adapter_mac: &str, device_mac: &str) -> Option<String> {
        let identity = mac_to_windows_format(device_mac);
        let mut candidates = vec![identity.clone()];
        if let Ok(keys) = self.open_bluetooth_le_keys() {
            if let Ok(adapter) = keys.open_subkey_with_flags(mac_to_windows_format(adapter_mac), KEY_READ) {
                if let Ok(Some(storage)) = Self::le_storage_name(&adapter, device_mac) {
                    if storage != identity { candidates.push(storage); }
                }
            }
        }
        if let Ok(root) = self.hklm.open_subkey_with_flags(BLUETOOTH_DEVICE_REG_PATH, KEY_READ) {
            for candidate in candidates {
                if let Ok(record) = root.open_subkey_with_flags(candidate, KEY_READ) {
                    if let Ok(value) = record.get_raw_value("Name") {
                        if let Some(name) = decode_cached_name(&value.bytes, value.vtype) {
                            if crate::bluetooth::useful_device_name(&name, device_mac) { return Some(name); }
                        }
                    }
                }
            }
        }
        self.read_pnp_name(device_mac)
    }

    /// BTHPORT caches a name only for peers that reported one while paired, so
    /// an old LE bond can have keys and no cached name. PnP still holds the
    /// friendly name Windows displays, and reading it changes no pairing state.
    /// Only device containers are read: per-service nodes name a profile.
    fn read_pnp_name(&self, device_mac: &str) -> Option<String> {
        let identity = mac_to_windows_format(device_mac);
        for path in PNP_ENUM_PATHS {
            let Ok(root) = self.hklm.open_subkey_with_flags(path, KEY_READ) else { continue; };
            let mut containers: Vec<String> = root.enum_keys().flatten()
                .filter(|device_id| device_id.to_ascii_uppercase().contains(&identity))
                .collect();
            containers.sort();
            for device_id in containers {
                let Ok(container) = root.open_subkey_with_flags(&device_id, KEY_READ) else { continue; };
                let mut instances: Vec<String> = container.enum_keys().flatten().collect();
                instances.sort();
                for instance in instances {
                    let Ok(node) = container.open_subkey_with_flags(&instance, KEY_READ) else { continue; };
                    let Ok(name) = node.get_value::<String, _>("FriendlyName") else { continue; };
                    let name = name.trim_end_matches('\0').trim().to_string();
                    if crate::bluetooth::useful_device_name(&name, device_mac) { return Some(name); }
                }
            }
        }
        None
    }
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
                log!(
                    "[BlueVein] LE registry path doesn't exist, attempting to create: {}",
                    BLUETOOTH_LE_REG_PATH
                );

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

    /// Windows can index an LE bond by its pairing-time RPA. Address/AddressType
    /// identify the peer that BlueZ must load the keys for, not the registry name.
    fn le_locations(adapter: &RegKey) -> Result<Vec<(String, String, Option<String>)>, Box<dyn Error>> {
        let mut locations = Vec::new();
        for name in adapter.enum_keys() {
            let name = name?;
            if !is_valid_mac_hex(&name) { continue; }
            let record = adapter.open_subkey_with_flags(&name, KEY_READ)?;
            let address = match record.get_value::<u64, _>("Address") {
                Ok(value) => Some(value),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            let address_type = match record.get_value::<u32, _>("AddressType") {
                Ok(0) => Some("public".to_string()),
                Ok(1) => Some("random".to_string()),
                Ok(_) => return Err("Unsupported Windows LE identity address type".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            let identity = if let Some(address) = address {
                if address == 0 || address > 0xffffffffffff {
                    return Err("Invalid Windows LE identity address".into());
                }
                if address_type.as_deref() == Some("random") && (address >> 40) & 0xc0 != 0xc0 {
                    return Err("Windows LE identity is a temporary random address".into());
                }
                windows_format_to_mac(&format!("{:012X}", address))
            } else {
                windows_format_to_mac(&name)
            };
            locations.push((name, identity, address_type));
        }
        Ok(locations)
    }

    fn le_storage_name(adapter: &RegKey, identity: &str) -> Result<Option<String>, Box<dyn Error>> {
        let identity = normalize_mac(identity);
        let mut candidates = Vec::new();
        for (name, peer, _) in Self::le_locations(adapter)? {
            if peer == identity {
                let record = adapter.open_subkey_with_flags(&name, KEY_READ)?;
                let ltk = record.get_raw_value("LTK").ok().map(|v| v.bytes);
                let irk = record.get_raw_value("IRK").ok().map(|v| v.bytes);
                candidates.push((name, ltk, irk));
            }
        }
        // Prefer a complete LE bond over a stale IRK-only record at the public MAC.
        if candidates.iter().any(|(_, ltk, _)| ltk.is_some()) {
            candidates.retain(|(_, ltk, _)| ltk.is_some());
        }
        if let Some(first) = candidates.first() {
            if candidates.iter().any(|other| other.1 != first.1 || other.2 != first.2) {
                let names: Vec<_> = candidates.iter().map(|c| c.0.clone()).collect();
                if let Some(selected) = unique_present_storage(&names, &present_le_storage_names()?) {
                    return Ok(Some(selected));
                }
                return Err(format!("Conflicting Windows LE records for {}; no unique present LE device", identity).into());
            }
        }
        candidates.sort_by_key(|entry| entry.0.to_ascii_uppercase());
        Ok(candidates.into_iter().next().map(|entry| entry.0))
    }

    /// Read classic Bluetooth device keys
    fn read_classic_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<Option<ClassicKeys>, Box<dyn Error>> {
        let bt_keys = self.open_bluetooth_keys()?;

        let adapter_key_name = mac_to_windows_format(adapter_mac);
        let device_key_name = mac_to_windows_format(device_mac);

        let adapter_key = match bt_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ) {
            Ok(key) => key,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
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
        let bt_le_keys = self.open_bluetooth_le_keys()?;

        let adapter_key_name = mac_to_windows_format(adapter_mac);

        let adapter_key = match bt_le_keys.open_subkey_with_flags(&adapter_key_name, KEY_READ) {
            Ok(key) => key,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let device_key_name = match Self::le_storage_name(&adapter_key, device_mac)? {
            Some(name) => name,
            None => return Ok(None),
        };
        let device_key = match adapter_key.open_subkey_with_flags(&device_key_name, KEY_READ) {
            Ok(key) => key,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let mut le_keys = LeKeys::default();
        let mut has_keys = false;
        le_keys.address_type = Self::le_locations(&adapter_key)?.into_iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(&device_key_name))
            .and_then(|(_, _, address_type)| address_type);

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
                let ediv = device_key
                    .get_value::<u32, _>("EDIV")
                    .ok()
                    .map(|v| v as u16);
                let rand = device_key.get_value::<u64, _>("ERand").ok();

                // BlueZ stores the MGMT key type in Authenticated: 0/1 legacy,
                // 2/3 Secure Connections. Windows uses a separate AuthReq SC bit.
                let authenticated = Some(windows_ltk_type(authenticated,
                    device_key.get_value::<u32, _>("AuthReq").ok(), ediv, rand));
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
                le_keys.irk_encoding = Some("windows".into());
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
        if let Some(ltk) = &le.ltk {
            validate_bluetooth_key(&ltk.key, "LTK")?;
            if ltk.authenticated_or_default() > 3 {
                return Err("Unsupported LTK security type for Windows".into());
            }
        }
        // Ensure base LE registry path exists (create if needed)
        let bt_le_keys = self.ensure_bluetooth_le_keys()?;

        let adapter_key_name = mac_to_windows_format(adapter_mac);

        // Create adapter key if it doesn't exist
        let (adapter_key, adapter_disp) =
            bt_le_keys.create_subkey(&adapter_key_name).map_err(|e| {
                format!(
                    "Failed to create adapter key {} in LE registry: {}",
                    adapter_key_name, e
                )
            })?;

        if adapter_disp == RegDisposition::REG_CREATED_NEW_KEY {
            log!(
                "[BlueVein]   Created new adapter key in LE registry: {}",
                adapter_key_name
            );
        }

        let device_key_name = Self::le_storage_name(&adapter_key, device_mac)?
            .unwrap_or_else(|| mac_to_windows_format(device_mac));
        // Create device key - this is where LE keys are stored
        let (device_key, device_disp) =
            adapter_key.create_subkey(&device_key_name).map_err(|e| {
                format!(
                    "Failed to create device key {} in LE registry: {}",
                    device_key_name, e
                )
            })?;

        if device_disp == RegDisposition::REG_CREATED_NEW_KEY {
            log!(
                "[BlueVein]   Created new device key in LE registry: {}",
                device_key_name
            );
        } else {
            log!(
                "[BlueVein]   Updating existing device key in LE registry: {}",
                device_key_name
            );
        }

        if let Some(address_type) = &le.address_type {
            let value = match address_type.as_str() {
                "public" => 0u32,
                "random" => 1u32,
                _ => return Err("Unsupported LE address type".into()),
            };
            let address = u64::from_str_radix(&mac_to_windows_format(device_mac), 16)?;
            device_key.set_value("Address", &address)?;
            device_key.set_value("AddressType", &value)?;
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
            let key_type = ltk.authenticated_or_default();
            if key_type > 3 { return Err("Unsupported LTK security type for Windows".into()); }
            device_key.set_value("Authenticated", &((key_type & 1) as u32))?;
            let auth_req = device_key.get_value::<u32, _>("AuthReq").unwrap_or(1);
            let auth_req = (auth_req & !0x0c) | if key_type & 2 != 0 { 0x08 } else { 0 }
                | if key_type & 1 != 0 { 0x04 } else { 0 };
            device_key.set_value("AuthReq", &auth_req)?;

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

            let csrk_bytes =
                hex::decode(&csrk_local.key).map_err(|e| format!("Invalid CSRK format: {}", e))?;

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

    /// Delete the BTHPORT key material this backend reads for one peer. Called
    /// only after the unpair API removed the device, or never knew it: the keys
    /// are what would otherwise resurrect the bond on the next synchronization.
    fn purge_registry_bond(&self, adapter_mac: &str, device_mac: &str) -> Result<(), Box<dyn Error>> {
        let keys = self.open_bluetooth_keys()?;
        let adapter = match keys.open_subkey_with_flags(mac_to_windows_format(adapter_mac), KEY_ALL_ACCESS) {
            Ok(key) => key,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match adapter.delete_value(mac_to_windows_format(device_mac)) {
            Ok(()) => log!("[BlueVein] Removed leftover Classic registry bond for {}", device_mac),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let identity = normalize_mac(device_mac);
        for (storage, peer, _) in Self::le_locations(&adapter)? {
            if peer != identity { continue; }
            adapter.delete_subkey_all(&storage)?;
            log!("[BlueVein] Removed leftover LE registry bond of {} stored as {}", device_mac, storage);
        }
        Ok(())
    }
}

impl BluetoothManager for WindowsBluetoothManager {
    fn platform_id(&self) -> &'static str { "windows" }
    fn update_reason(&self, current: &BluetoothDevice, desired: &BluetoothDevice) -> Option<String> {
        let (Some(local), Some(shared)) = (&current.le, &desired.le) else { return None; };
        if let (Some(local_irk), Some(shared_irk)) = (&local.irk, &shared.irk) {
            if !local_irk.eq_ignore_ascii_case(shared_irk)
                && shared.irk_encoding.as_deref() != Some("windows") {
                return Some("conflicting legacy IRK encoding; preserving live Windows bond".into());
            }
        }
        None
    }

    fn migrate_shared_config(&self, config: &mut crate::config::BlueVeinConfig) -> Result<(), Box<dyn Error>> {
        for adapter_mac in self.get_adapters()? {
            let keys = self.open_bluetooth_keys()?;
            let adapter = keys.open_subkey_with_flags(mac_to_windows_format(&adapter_mac), KEY_READ)?;
            for local in self.get_devices(&adapter_mac)? {
                let Some(shared) = config.get_device(&adapter_mac, &local.mac_address).cloned() else { continue; };
                if shared.pending_deletion.is_some() { continue; }
                let Some(selected) = Self::le_storage_name(&adapter, &local.mac_address)? else { continue; };
                let names: Vec<_> = Self::le_locations(&adapter)?.into_iter()
                    .filter(|(_, peer, _)| peer == &local.mac_address).map(|(name, _, _)| name).collect();
                if names.len() < 2 { continue; }
                let present = present_le_storage_names()?;
                if unique_present_storage(&names, &present).as_deref() != Some(selected.as_str()) { continue; }
                let (Some(live), Some(old)) = (&local.le, &shared.le) else { continue; };
                let (Some(live_ltk), Some(old_ltk), Some(live_irk), Some(old_irk)) =
                    (&live.ltk, &old.ltk, &live.irk, &old.irk) else { continue; };
                if live_ltk.key.eq_ignore_ascii_case(&old_ltk.key) && live_irk.eq_ignore_ascii_case(old_irk) { continue; }
                if old.irk_encoding.as_deref() != Some("windows") { continue; }
                let mut verified_shadow = false;
                for name in names.iter().filter(|name| *name != &selected) {
                    let record = adapter.open_subkey_with_flags(name, KEY_READ)?;
                    if let (Ok(ltk), Ok(irk)) = (record.get_raw_value("LTK"), record.get_raw_value("IRK")) {
                        if hex::encode(&ltk.bytes).eq_ignore_ascii_case(&old_ltk.key)
                            && hex::encode(&irk.bytes).eq_ignore_ascii_case(old_irk) {
                            verified_shadow = true;
                        }
                    }
                }
                if verified_shadow {
                    if old.peripheral_ltk.is_some() && !matches!(live_ltk.authenticated, Some(2) | Some(3)) {
                        return Err("Cannot replace a legacy peripheral key after re-pairing".into());
                    }
                    let mut repaired = shared.clone();
                    let exported = self.prepare_local_export(&local, &shared);
                    repaired.le = exported.le;
                    // A Windows re-pair can renew Classic and LE together.
                    // Preserve its live Classic bond instead of importing stale EFI.
                    if exported.classic.is_some() { repaired.classic = exported.classic; }
                    repaired.name = self.choose_shared_name(&local, &shared);
                    config.update_device(adapter_mac.clone(), repaired);
                    log!("[BlueVein] Recovered re-paired LE identity {} from its unique present Windows device; EFI matched an inactive shadow", local.mac_address);
                }
            }
            // Registry reads establish the format of matching legacy IRKs.
            for local in self.get_devices(&adapter_mac)? {
                if let Some(shared) = config.adapters.get_mut(&adapter_mac).and_then(|a| a.devices.get_mut(&local.mac_address)) {
                    if let (Some(a), Some(b)) = (&local.le, &mut shared.le) {
                        if b.irk_encoding.is_none() && b.irk.is_some() && a.irk.as_ref().zip(b.irk.as_ref()).is_some_and(|(x,y)| x.eq_ignore_ascii_case(y)) {
                            b.irk_encoding = Some("windows".into());
                        }
                    }
                }
            }
            let keys = self.open_bluetooth_keys()?;
            let adapter = keys.open_subkey_with_flags(mac_to_windows_format(&adapter_mac), KEY_READ)?;
            for (storage, identity, _) in Self::le_locations(&adapter)? {
                if let Ok(filter) = std::env::var("BLUEVEIN_DEVICE_FILTER") {
                    if identity != normalize_mac(&filter) { continue; }
                }
                let alias = windows_format_to_mac(&storage);
                if alias == identity { continue; }
                let Some(old_alias) = config.get_device(&adapter_mac, &alias).cloned() else { continue; };
                let current = self.get_device(&adapter_mac, &identity)?;
                let live_le = current.le.as_ref().ok_or("Missing live LE bond during identity migration")?;
                let old_le = old_alias.le.as_ref().ok_or("Legacy alias has no LE bond")?;
                let same_ltk = match (&old_le.ltk, &live_le.ltk) {
                    (Some(a), Some(b)) => a.key.eq_ignore_ascii_case(&b.key)
                        && a.ediv == b.ediv && a.rand == b.rand && a.enc_size == b.enc_size,
                    _ => false,
                };
                let same_irk = match (&old_le.irk, &live_le.irk) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => false,
                };
                if !same_ltk || !same_irk {
                    return Err("Legacy EFI alias differs from the live Windows bond; refusing automatic migration".into());
                }
                let mut canonical = config.get_device(&adapter_mac, &identity).cloned()
                    .unwrap_or_else(|| current.clone());
                let previous_peripheral = canonical.le.as_ref().and_then(|le| le.peripheral_ltk.as_ref());
                let mut new_le = live_le.clone();
                if let Some(old_peripheral) = previous_peripheral {
                    let ltk = live_le.ltk.as_ref().ok_or("Missing LTK")?;
                    if matches!(ltk.authenticated, Some(2) | Some(3)) {
                        // SC has one key for both roles; replace the stale old SC/legacy key.
                        // Keeping it would let Linux continue selecting a rejected bond.
                        new_le.peripheral_ltk = Some(ltk.clone());
                    } else if old_peripheral.key.eq_ignore_ascii_case(&ltk.key) {
                        new_le.peripheral_ltk = Some(old_peripheral.clone());
                    } else {
                        return Err("Legacy role-specific LTK conflict requires explicit resolution".into());
                    }
                }
                canonical.mac_address = identity.clone();
                canonical.le = Some(new_le);
                if canonical.classic.is_none() { canonical.classic = current.classic; }
                config.update_device(adapter_mac.clone(), canonical);
                config.adapters.get_mut(&adapter_mac).unwrap().devices.remove(&alias);
                log!("[BlueVein] Migrated verified LE alias {} to identity {}", alias, identity);
            }
        }
        Ok(())
    }

    fn prepare_local_export(&self, local: &BluetoothDevice, shared: &BluetoothDevice) -> BluetoothDevice {
        let mut result = local.clone();
        if let (Some(local), Some(shared)) = (&mut result.classic, &shared.classic) {
            if local.link_key.eq_ignore_ascii_case(&shared.link_key) {
                local.key_type = shared.key_type;
                local.pin_length = shared.pin_length;
            }
        }
        if let (Some(local), Some(shared)) = (&mut result.le, &shared.le) {
            if let Some(ltk) = local.ltk.as_ref() {
                if matches!(ltk.authenticated, Some(2) | Some(3)) && shared.peripheral_ltk.is_some() {
                    local.peripheral_ltk = Some(ltk.clone());
                }
            }
        }
        result
    }

    fn choose_shared_name(&self, local: &BluetoothDevice, shared: &BluetoothDevice) -> Option<String> {
        local.name.clone().or_else(|| shared.name.clone())
    }

    fn needs_update(&self, current: &BluetoothDevice, desired: &BluetoothDevice) -> bool {
        registry_projection(current) != registry_projection(desired)
    }

    fn get_adapters(&self) -> Result<Vec<String>, Box<dyn Error>> {
        // Classic and LE use the same root. An unreadable root is not an empty
        // Bluetooth configuration: propagate failure rather than importing over it.
        let keys = self.open_bluetooth_keys()?;
        let mut adapters = Vec::new();
        for name in keys.enum_keys() {
            let name = name?;
            if is_valid_mac_hex(&name) {
                adapters.push(windows_format_to_mac(&name));
            }
        }
        if let Ok(filter) = std::env::var("BLUEVEIN_ADAPTER_FILTER") {
            adapters.retain(|adapter| *adapter == normalize_mac(&filter));
        }
        Ok(adapters)
    }

    fn get_devices(&self, adapter_mac: &str) -> Result<Vec<BluetoothDevice>, Box<dyn Error>> {
        let keys = self.open_bluetooth_keys()?;
        let adapter = keys.open_subkey_with_flags(mac_to_windows_format(adapter_mac), KEY_READ)?;
        let mut names = std::collections::BTreeSet::new();
        for value in adapter.enum_values() {
            let (name, _) = value?;
            if is_valid_mac_hex(&name) { names.insert(windows_format_to_mac(&name)); }
        }
        for (_, identity, _) in Self::le_locations(&adapter)? {
            names.insert(identity);
        }
        let mut devices = Vec::new();
        for mac in names {
            let classic = self.read_classic_device(adapter_mac, &mac)?;
            let le = self.read_le_device(adapter_mac, &mac)?;
            if classic.is_some() || le.is_some() {
                let name = self.read_device_name(adapter_mac, &mac);
                devices.push(BluetoothDevice { mac_address: mac, name, pending_deletion: None, classic, le });
            }
        }
        if let Ok(filter) = std::env::var("BLUEVEIN_DEVICE_FILTER") {
            devices.retain(|device| device.mac_address == normalize_mac(&filter));
        }
        Ok(devices)
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
            name: self.read_device_name(adapter_mac, device_mac),
            pending_deletion: None,
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

    fn remove_device(&mut self, adapter_mac: &str, device_mac: &str) -> Result<(), Box<dyn Error>> {
        // Use the Windows unpair API so PnP state and cached services are
        // removed along with keys. Registry deletion alone is not an unpair.
        let address = u64::from_str_radix(&mac_to_windows_format(device_mac), 16)?;
        #[link(name = "Bthprops")]
        extern "system" {
            fn BluetoothRemoveDevice(address: *const u64) -> u32;
        }
        let result = unsafe { BluetoothRemoveDevice(&address) };
        match result {
            0 => {}
            // The stack has no device object, yet BlueVein reads this bond from
            // BTHPORT. Only the stale key material is left to remove; failing
            // here would keep re-importing a pairing that Windows already lost.
            ERROR_NOT_FOUND => log!(
                "[BlueVein] Windows has no paired device object for {}; removing its leftover registry bond",
                device_mac
            ),
            error => {
                return Err(format!("Windows could not unpair {} (error {})", device_mac, error).into())
            }
        }
        self.purge_registry_bond(adapter_mac, device_mac)
    }
}

fn windows_ltk_type(authenticated: Option<u8>, auth_req: Option<u32>, ediv: Option<u16>, rand: Option<u64>) -> u8 {
    let authenticated = authenticated.unwrap_or(0);
    let secure = auth_req.map(|flags| flags & 0x08 != 0).unwrap_or(false)
        && ediv == Some(0) && rand == Some(0);
    // Do not treat requested MITM in AuthReq as proof of authenticated pairing.
    (authenticated & 1) | if secure || authenticated & 2 != 0 { 2 } else { 0 }
}

fn decode_cached_name(bytes: &[u8], kind: RegType) -> Option<String> {
    let string_value = matches!(kind, RegType::REG_SZ | RegType::REG_EXPAND_SZ);
    if !string_value {
        if let Ok(value) = std::str::from_utf8(bytes) {
            let value = value.trim_end_matches('\0').trim();
            if !value.is_empty() && !value.chars().any(char::is_control) {
                return Some(value.to_string());
            }
        }
    }
    let utf16 = string_value || bytes.len() % 2 == 0;
    let name = if utf16 {
        if bytes.len() % 2 != 0 { return None; }
        let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        String::from_utf16(&units).ok()?
    } else {
        String::from_utf8(bytes.to_vec()).ok()?
    };
    let name = name.trim_matches('\0').trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The values that survive a Windows registry write/read round trip.
/// Do not alias peripheral_ltk to ltk: their role semantics are different.
fn registry_projection(device: &BluetoothDevice) -> BluetoothDevice {
    let mut result = device.clone();
    result.name = None;
    result.pending_deletion = None;
    if let Some(classic) = result.classic.as_mut() {
        classic.link_key.make_ascii_uppercase();
        classic.key_type = 4;
        classic.pin_length = 0;
    }
    if let Some(le) = result.le.as_mut() {
        if let Some(irk) = le.irk.as_mut() { irk.make_ascii_uppercase(); }
        le.irk_encoding = None;
        le.peripheral_ltk = None;
        if let Some(ltk) = le.ltk.as_mut() {
            ltk.key.make_ascii_uppercase();
            ltk.authenticated = Some(ltk.authenticated_or_default());
        }
        for csrk in [&mut le.csrk_local, &mut le.csrk_remote].into_iter().flatten() {
            csrk.key.make_ascii_uppercase();
            csrk.counter = 0;
            csrk.authenticated = false;
        }
        if le.ltk.is_none() && le.irk.is_none() && le.csrk_local.is_none() && le.csrk_remote.is_none() {
            result.le = None;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_startup_and_monitor_export_repaired_bond_without_manual_repair() {
        use crate::{config::BlueVeinConfig, efi::{ConfigStore, EfiError}, sync::SyncManager};
        use std::sync::{Arc, Mutex};
        struct Store(Arc<Mutex<(BlueVeinConfig, usize)>>);
        impl ConfigStore for Store {
            fn read(&self) -> Result<BlueVeinConfig, EfiError> { Ok(self.0.lock().unwrap().0.clone()) }
            fn write(&mut self, config: &BlueVeinConfig) -> Result<(), EfiError> {
                let mut state = self.0.lock().unwrap(); state.0 = config.clone(); state.1 += 1; Ok(())
            }
            fn display_name(&self) -> &str { "test memory" }
        }
        struct Reset;
        impl Drop for Reset { fn drop(&mut self) { TEST_PRESENT_LE.with(|v| *v.borrow_mut() = None); } }
        TEST_PRESENT_LE.with(|v| *v.borrow_mut() = Some(vec!["412233445566".into()]));
        let _reset = Reset;
        let (hkcu, root, manager) = identity_fixture("automatic-repair");
        let keys = manager.open_bluetooth_keys().unwrap();
        let adapter = keys.open_subkey_with_flags("001122334455", KEY_ALL_ACCESS).unwrap();
        let alias = adapter.open_subkey_with_flags("412233445566", KEY_ALL_ACCESS).unwrap();
        let shadow = adapter.open_subkey_with_flags("123456789abc", KEY_ALL_ACCESS).unwrap();
        shadow.set_raw_value("LTK", &winreg::RegValue { bytes: vec![0x99; 16], vtype: RegType::REG_BINARY }).unwrap();
        let live = manager.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap();
        let mut old = live.clone();
        old.le.as_mut().unwrap().ltk.as_mut().unwrap().key = "99".repeat(16);
        old.le.as_mut().unwrap().irk = Some("99".repeat(16));
        old.le.as_mut().unwrap().peripheral_ltk = old.le.as_ref().unwrap().ltk.clone();
        let mut shared = BlueVeinConfig::new(); shared.update_device("00:11:22:33:44:55".into(), old);
        let state = Arc::new(Mutex::new((shared, 0)));
        let mut sync = SyncManager::with_test_store(Box::new(manager), Box::new(Store(state.clone())));
        // This is the normal service startup path, not repair-efi-only.
        sync.sync_bidirectional().unwrap();
        assert_eq!(state.lock().unwrap().0.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap().le.as_ref().unwrap().ltk, live.le.as_ref().unwrap().ltk);
        let before = sync.local_snapshot().unwrap();
        // Simulate another Windows re-pair while the service is running.
        alias.set_raw_value("LTK", &winreg::RegValue { bytes: vec![0x55; 16], vtype: RegType::REG_BINARY }).unwrap();
        alias.set_raw_value("IRK", &winreg::RegValue { bytes: vec![0x66; 16], vtype: RegType::REG_BINARY }).unwrap();
        let current = sync.local_snapshot().unwrap();
        let changes = super::super::monitor::changed_devices(&before, &current);
        assert_eq!(changes.len(), 1);
        for (adapter, peer) in changes { sync.handle_device_change(&adapter, &peer).unwrap(); }
        let writes = state.lock().unwrap().1;
        for _ in 0..3 { sync.check_efi_changes().unwrap(); }
        assert_eq!(sync.local_snapshot().unwrap(), current);
        let stored = state.lock().unwrap();
        let le = stored.0.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap().le.as_ref().unwrap();
        assert_eq!(le.ltk.as_ref().unwrap().key, "55".repeat(16));
        assert_eq!(le.irk, Some("66".repeat(16)));
        assert_eq!(le.peripheral_ltk, le.ltk);
        assert_eq!(stored.1, writes);
        assert_eq!(shadow.get_raw_value("LTK").unwrap().bytes, vec![0x99; 16]);
        drop(stored); drop(sync); hkcu.delete_subkey_all(root).unwrap();
    }

    #[test]
    fn re_pair_migrates_only_efi_shadow_without_changing_windows_keys() {
        struct Reset;
        impl Drop for Reset { fn drop(&mut self) { TEST_PRESENT_LE.with(|v| *v.borrow_mut() = None); } }
        TEST_PRESENT_LE.with(|v| *v.borrow_mut() = Some(vec!["412233445566".into()]));
        let _reset = Reset;
        let (hkcu, root, manager) = identity_fixture("repaired-shadow");
        let keys = manager.open_bluetooth_keys().unwrap();
        let adapter = keys.open_subkey_with_flags("001122334455", KEY_ALL_ACCESS).unwrap();
        let shadow = adapter.open_subkey_with_flags("123456789abc", KEY_ALL_ACCESS).unwrap();
        shadow.set_raw_value("LTK", &winreg::RegValue { bytes: vec![0x99; 16], vtype: RegType::REG_BINARY }).unwrap();
        adapter.set_raw_value("123456789abc", &winreg::RegValue { bytes: vec![0x44; 16], vtype: RegType::REG_BINARY }).unwrap();
        let live = manager.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap();
        let mut old = live.clone();
        old.classic.as_mut().unwrap().link_key = "77".repeat(16);
        let le = old.le.as_mut().unwrap();
        le.ltk.as_mut().unwrap().key = "99".repeat(16);
        le.irk = Some("99".repeat(16));
        le.peripheral_ltk = le.ltk.clone();
        let mut config = crate::config::BlueVeinConfig::new();
        config.update_device("00:11:22:33:44:55".into(), old);
        manager.migrate_shared_config(&mut config).unwrap();
        let repaired = config.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap();
        assert_eq!(repaired.classic, live.classic);
        assert!(!manager.needs_update(&live, repaired));
        assert_eq!(adapter.get_raw_value("123456789abc").unwrap().bytes, vec![0x44; 16]);
        assert_eq!(repaired.le.as_ref().unwrap().ltk, live.le.as_ref().unwrap().ltk);
        assert_eq!(repaired.le.as_ref().unwrap().irk, live.le.as_ref().unwrap().irk);
        assert_eq!(repaired.le.as_ref().unwrap().peripheral_ltk, live.le.as_ref().unwrap().ltk);
        assert_eq!(shadow.get_raw_value("LTK").unwrap().bytes, vec![0x99; 16]);
        assert_eq!(adapter.open_subkey("412233445566").unwrap().get_raw_value("LTK").unwrap().bytes, vec![0x11; 16]);
        let once = config.clone();
        manager.migrate_shared_config(&mut config).unwrap();
        assert_eq!(config, once);
        // An unrelated EFI bond must not be silently replaced.
        config.adapters.get_mut("00:11:22:33:44:55").unwrap().devices.get_mut("12:34:56:78:9A:BC").unwrap().le.as_mut().unwrap().ltk.as_mut().unwrap().key = "77".repeat(16);
        let unrelated = config.clone();
        manager.migrate_shared_config(&mut config).unwrap();
        assert_eq!(config, unrelated);
        drop(manager);
        hkcu.delete_subkey_all(root).unwrap();
    }

    #[test]
    fn unique_present_le_record_resolves_shadow_but_not_two_live_bonds() {
        let names = vec!["123456789ABC".into(), "412233445566".into()];
        assert_eq!(unique_present_storage(&names, &["412233445566".into()]), Some("412233445566".into()));
        assert_eq!(unique_present_storage(&names, &names), None);
        assert_eq!(unique_present_storage(&names, &[]), None);
        assert_eq!(unique_present_storage(&names, &["123456789abc".into()]), Some("123456789ABC".into()));
        assert_eq!(unique_present_storage(&names, &["FFFFFFFFFFFF".into()]), None);
    }

    #[test]
    fn cached_name_decodes_binary_utf8_and_registry_utf16() {
        assert_eq!(decode_cached_name(b"MX Keys\0", RegType::REG_BINARY).as_deref(), Some("MX Keys"));
        let wide: Vec<u8> = "InpuDeck\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_cached_name(&wide, RegType::REG_SZ).as_deref(), Some("InpuDeck"));
    }

    #[test]
    fn untagged_conflicting_irk_cannot_overwrite_live_windows_bond() {
        let manager = WindowsBluetoothManager::new().unwrap();
        let mut local = BluetoothDevice::le_with_ltk("AA:BB:CC:DD:EE:FF".into(), key());
        local.le.as_mut().unwrap().irk = Some("22".repeat(16));
        local.le.as_mut().unwrap().irk_encoding = Some("windows".into());
        let mut shared = local.clone();
        shared.le.as_mut().unwrap().irk = Some("33".repeat(16));
        shared.le.as_mut().unwrap().irk_encoding = None;
        assert!(manager.update_reason(&local, &shared).is_some());
        shared.le.as_mut().unwrap().irk_encoding = Some("windows".into());
        assert!(manager.update_reason(&local, &shared).is_none());
    }

    #[test]
    fn windows_registry_round_trip_converges_without_touching_system_bonds() {
        // Isolated HKCU fixture. The normal SYSTEM path is nested below this key.
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let root = format!("Software\\BlueVeinTests\\roundtrip-{}", std::process::id());
        let (fixture, _) = hkcu.create_subkey(&root).unwrap();
        let mut manager = WindowsBluetoothManager { hklm: fixture };
        let adapter = "00:11:22:33:44:55";
        let mut desired = BluetoothDevice::le_with_ltk("AA:BB:CC:DD:EE:FF".into(), key());
        let le = desired.le.as_mut().unwrap();
        le.irk = Some("22".repeat(16));
        le.peripheral_ltk = Some(key());
        le.address_type = Some("public".into());
        le.csrk_local = Some(CsrkKey { key: "33".repeat(16), counter: 100, authenticated: true });
        manager.set_device(adapter, &desired).unwrap();
        let actual = manager.get_device(adapter, &desired.mac_address).unwrap();
        assert!(!manager.needs_update(&actual, &desired));
        assert_eq!(actual.le.as_ref().unwrap().ltk, desired.le.as_ref().unwrap().ltk);
        assert!(actual.le.as_ref().unwrap().peripheral_ltk.is_none());
        assert_eq!(manager.get_devices(adapter).unwrap().len(), 1);
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }
    #[test]
    fn hexadecimal_case_is_not_a_key_change() {
        let mut lower = BluetoothDevice::le_with_ltk("AA:BB:CC:DD:EE:FF".into(), key());
        let le = lower.le.as_mut().unwrap();
        le.ltk.as_mut().unwrap().key = "ab".repeat(16);
        le.irk = Some("cd".repeat(16));
        let upper = registry_projection(&lower);
        assert_eq!(registry_projection(&lower), registry_projection(&upper));
    }
    #[test]
    fn le_rekey_does_not_downgrade_unchanged_classic_metadata() {
        let manager = WindowsBluetoothManager::new().unwrap();
        let mut local = BluetoothDevice::classic("AA:BB:CC:DD:EE:FF".into(), "ab".repeat(16));
        let mut shared = local.clone();
        shared.classic.as_mut().unwrap().key_type = 8;
        shared.classic.as_mut().unwrap().pin_length = 6;
        let exported = manager.prepare_local_export(&local, &shared);
        assert_eq!(exported.classic.unwrap().key_type, 8);
        local.classic.as_mut().unwrap().link_key = "cd".repeat(16);
        assert_eq!(manager.prepare_local_export(&local, &shared).classic.unwrap().key_type, 4);
    }


    fn identity_fixture(label: &str) -> (RegKey, String, WindowsBluetoothManager) {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let root = format!("Software\\BlueVeinTests\\{}-{}", label, std::process::id());
        let (fixture, _) = hkcu.create_subkey(&root).unwrap();
        let mut manager = WindowsBluetoothManager { hklm: fixture };
        let mut live = BluetoothDevice::le_with_ltk("41:22:33:44:55:66".into(), key());
        live.le.as_mut().unwrap().irk = Some("22".repeat(16));
        manager.set_device("00:11:22:33:44:55", &live).unwrap();
        let keys = manager.open_bluetooth_keys().unwrap();
        let adapter = keys.open_subkey_with_flags("001122334455", KEY_ALL_ACCESS).unwrap();
        let alias = adapter.open_subkey_with_flags("412233445566", KEY_ALL_ACCESS).unwrap();
        alias.set_value("Address", &0x123456789abcu64).unwrap();
        alias.set_value("AddressType", &0u32).unwrap();
        alias.set_value("AuthReq", &0x2du32).unwrap();
        alias.delete_value("Authenticated").unwrap();
        let (shadow, _) = adapter.create_subkey("123456789abc").unwrap();
        shadow.set_raw_value("IRK", &winreg::RegValue { bytes: vec![0x99; 16], vtype: RegType::REG_BINARY }).unwrap();
        (hkcu, root, manager)
    }

    #[test]
    fn rpa_registry_record_exports_under_identity_and_imports_to_same_storage() {
        let (hkcu, root, mut manager) = identity_fixture("identity-roundtrip");
        let mut devices = manager.get_devices("00:11:22:33:44:55").unwrap();
        assert_eq!(devices.len(), 1);
        let mut device = devices.pop().unwrap();
        assert_eq!(device.mac_address, "12:34:56:78:9A:BC");
        assert_eq!(device.le.as_ref().unwrap().address_type.as_deref(), Some("public"));
        assert_eq!(device.le.as_ref().unwrap().ltk.as_ref().unwrap().authenticated, Some(2));
        assert_eq!(device.le.as_ref().unwrap().irk.as_deref(), Some("22222222222222222222222222222222"));
        device.le.as_mut().unwrap().ltk.as_mut().unwrap().key = "33".repeat(16);
        manager.set_device("00:11:22:33:44:55", &device).unwrap();
        assert!(!manager.needs_update(&manager.get_device("00:11:22:33:44:55", &device.mac_address).unwrap(), &device));
        let keys = manager.open_bluetooth_keys().unwrap();
        let adapter = keys.open_subkey("001122334455").unwrap();
        assert_eq!(adapter.open_subkey("412233445566").unwrap().get_raw_value("LTK").unwrap().bytes, vec![0x33; 16]);
        assert!(adapter.open_subkey("123456789abc").unwrap().get_raw_value("LTK").is_err());
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }

    #[test]
    fn pnp_friendly_name_fills_a_bond_without_a_bthport_cache_entry() {
        let (hkcu, root, manager) = identity_fixture("pnp-name");
        assert!(manager.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap().name.is_none());
        let (container, _) = manager.hklm
            .create_subkey(r"SYSTEM\CurrentControlSet\Enum\BTHLE\Dev_123456789ABC").unwrap();
        let (node, _) = container.create_subkey("9&35dc57f9&0&123456789ABC").unwrap();
        node.set_value("FriendlyName", &"IINE GAMEPAD").unwrap();
        // A per-service node must not outrank the device container's own name.
        let (service, _) = manager.hklm
            .create_subkey(r"SYSTEM\CurrentControlSet\Enum\BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_LOCALMFG&0000").unwrap();
        let (service_node, _) = service.create_subkey("7&1a2b3c&0&123456789ABC_C00000000").unwrap();
        service_node.set_value("FriendlyName", &"Audio sink").unwrap();
        assert_eq!(manager.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap().name.as_deref(),
            Some("IINE GAMEPAD"));
        // The address itself is not a name, and another peer keeps its own.
        node.set_value("FriendlyName", &"12:34:56:78:9A:BC").unwrap();
        assert!(manager.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap().name.is_none());
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }

    #[test]
    fn leftover_registry_bond_is_purged_when_windows_lost_the_device_object() {
        let (hkcu, root, manager) = identity_fixture("leftover-bond");
        let keys = manager.open_bluetooth_keys().unwrap();
        let adapter = keys.open_subkey_with_flags("001122334455", KEY_ALL_ACCESS).unwrap();
        adapter.set_raw_value("123456789ABC", &winreg::RegValue {
            bytes: vec![0x44; 16], vtype: RegType::REG_BINARY,
        }).unwrap();
        let other = BluetoothDevice::classic("AA:BB:CC:DD:EE:FF".into(), "55".repeat(16));
        adapter.set_raw_value("AABBCCDDEEFF", &winreg::RegValue {
            bytes: hex::decode(&other.classic.as_ref().unwrap().link_key).unwrap(), vtype: RegType::REG_BINARY,
        }).unwrap();
        manager.purge_registry_bond("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap();
        assert!(adapter.get_raw_value("123456789ABC").is_err());
        assert!(adapter.open_subkey("412233445566").is_err());
        assert!(adapter.open_subkey("123456789abc").is_err());
        let devices = manager.get_devices("00:11:22:33:44:55").unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].mac_address, other.mac_address);
        // Purging an unknown peer is not an error and changes nothing.
        manager.purge_registry_bond("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap();
        assert_eq!(manager.get_devices("00:11:22:33:44:55").unwrap().len(), 1);
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }

    #[test]
    fn migration_moves_verified_alias_keys_and_replaces_stale_sc_role_key() {
        let (hkcu, root, manager) = identity_fixture("identity-migration");
        let mut config = crate::config::BlueVeinConfig::new();
        let mut alias = BluetoothDevice::le_with_ltk("41:22:33:44:55:66".into(), key());
        alias.le.as_mut().unwrap().irk = Some("22".repeat(16));
        config.update_device("00:11:22:33:44:55".into(), alias);
        let mut old = BluetoothDevice::classic("12:34:56:78:9A:BC".into(), "44".repeat(16));
        let mut old_key = key(); old_key.key = "99".repeat(16); old_key.authenticated = Some(2);
        old.le = Some(LeKeys { peripheral_ltk: Some(old_key), irk: Some("99".repeat(16)), ..Default::default() });
        config.update_device("00:11:22:33:44:55".into(), old);
        let other = BluetoothDevice::classic("AA:BB:CC:DD:EE:FF".into(), "55".repeat(16));
        config.update_device("00:11:22:33:44:55".into(), other.clone());
        manager.migrate_shared_config(&mut config).unwrap();
        assert!(config.get_device("00:11:22:33:44:55", "41:22:33:44:55:66").is_none());
        let result = config.get_device("00:11:22:33:44:55", "12:34:56:78:9A:BC").unwrap();
        let le = result.le.as_ref().unwrap();
        assert_eq!(le.ltk, le.peripheral_ltk);
        assert_eq!(le.ltk.as_ref().unwrap().authenticated, Some(2));
        assert_eq!(result.classic.as_ref().unwrap().link_key, "44".repeat(16));
        assert_eq!(config.get_device("00:11:22:33:44:55", &other.mac_address), Some(&other));
        let once = config.clone();
        manager.migrate_shared_config(&mut config).unwrap();
        assert_eq!(once, config);
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }

    #[test]
    fn migration_rejects_unverified_alias_without_changing_shared_config() {
        let (hkcu, root, manager) = identity_fixture("identity-conflict");
        let mut config = crate::config::BlueVeinConfig::new();
        let mut alias = BluetoothDevice::le_with_ltk("41:22:33:44:55:66".into(), key());
        alias.le.as_mut().unwrap().irk = Some("88".repeat(16));
        config.update_device("00:11:22:33:44:55".into(), alias);
        let before = config.clone();
        assert!(manager.migrate_shared_config(&mut config).is_err());
        assert_eq!(before, config);
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }

    #[test]
    fn local_sc_rekey_replaces_stale_shared_peripheral_key() {
        let manager = WindowsBluetoothManager::new().unwrap();
        let mut local = BluetoothDevice::le_with_ltk("12:34:56:78:9A:BC".into(), key());
        let mut shared = local.clone();
        shared.le.as_mut().unwrap().peripheral_ltk = Some(key());
        for security_type in [2, 3] {
            let ltk = local.le.as_mut().unwrap().ltk.as_mut().unwrap();
            ltk.key = "77".repeat(16);
            ltk.authenticated = Some(security_type);
            let prepared = manager.prepare_local_export(&local, &shared);
            let result = shared.merge_with(&prepared);
            let le = result.le.unwrap();
            assert_eq!(le.peripheral_ltk, le.ltk);
            assert_eq!(le.ltk.unwrap().key, "77".repeat(16));
        }
        // A legacy key cannot replace a different role's key.
        local.le.as_mut().unwrap().ltk.as_mut().unwrap().authenticated = Some(1);
        let result = shared.merge_with(&manager.prepare_local_export(&local, &shared));
        assert_eq!(result.le.unwrap().peripheral_ltk, shared.le.unwrap().peripheral_ltk);
    }

    #[test]
    fn conflicting_complete_identity_bonds_refuse_reads_and_imports() {
        let (hkcu, root, mut manager) = identity_fixture("complete-conflict");
        let keys = manager.open_bluetooth_keys().unwrap();
        let adapter = keys.open_subkey_with_flags("001122334455", KEY_ALL_ACCESS).unwrap();
        let shadow = adapter.open_subkey_with_flags("123456789abc", KEY_ALL_ACCESS).unwrap();
        shadow.set_raw_value("LTK", &winreg::RegValue {
            bytes: vec![0x88; 16], vtype: RegType::REG_BINARY,
        }).unwrap();
        assert!(manager.get_devices("00:11:22:33:44:55").is_err());
        let desired = BluetoothDevice::le_with_ltk("12:34:56:78:9A:BC".into(), key());
        assert!(manager.set_device("00:11:22:33:44:55", &desired).is_err());
        assert_eq!(shadow.get_raw_value("LTK").unwrap().bytes, vec![0x88; 16]);
        assert_eq!(adapter.open_subkey("412233445566").unwrap().get_raw_value("LTK").unwrap().bytes, vec![0x11; 16]);
        drop(manager);
        hkcu.delete_subkey_all(&root).unwrap();
    }

    #[test]
    fn sc_security_type_is_not_inferred_from_requested_mitm() {
        assert_eq!(windows_ltk_type(None, Some(0x2d), Some(0), Some(0)), 2);
        assert_eq!(windows_ltk_type(Some(1), Some(0x2d), Some(0), Some(0)), 3);
        assert_eq!(windows_ltk_type(Some(1), Some(0x05), Some(7), Some(9)), 1);
        assert_eq!(windows_ltk_type(None, None, Some(0), Some(0)), 0);
    }

    fn key() -> LeLongTermKey {
        LeLongTermKey { key: "11".repeat(16), authenticated: Some(1),
            enc_size: Some(16), ediv: Some(0), rand: Some(0) }
    }

    #[test]
    fn iphone_irk_only_does_not_reimport_linux_metadata() {
        let current = BluetoothDevice { mac_address: "AA:BB:CC:DD:EE:FF".into(), name: None, pending_deletion: None,
            classic: None, le: Some(LeKeys { irk: Some("22".repeat(16)), ..Default::default() }) };
        let mut desired = current.clone();
        desired.le.as_mut().unwrap().peripheral_ltk = Some(key());
        assert_eq!(registry_projection(&current), registry_projection(&desired));
        assert!(desired.le.as_ref().unwrap().peripheral_ltk.is_some());
    }

    #[test]
    fn real_ltk_and_irk_changes_still_require_import() {
        let current = BluetoothDevice::le_with_ltk("AA:BB:CC:DD:EE:FF".into(), key());
        let mut desired = current.clone();
        desired.le.as_mut().unwrap().ltk.as_mut().unwrap().key = "33".repeat(16);
        assert_ne!(registry_projection(&current), registry_projection(&desired));
        let mut desired = current.clone();
        desired.le.as_mut().unwrap().irk = Some("22".repeat(16));
        assert_ne!(registry_projection(&current), registry_projection(&desired));
    }

    #[test]
    fn peripheral_key_alone_is_not_a_windows_ltk() {
        let mut device = BluetoothDevice::le_with_ltk("AA:BB:CC:DD:EE:FF".into(), key());
        let le = device.le.as_mut().unwrap();
        le.peripheral_ltk = le.ltk.take();
        assert!(registry_projection(&device).le.is_none());
    }
}

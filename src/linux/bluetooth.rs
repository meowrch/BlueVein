use crate::bluetooth::{
    normalize_mac, useful_device_name, validate_bluetooth_key, BluetoothDevice, BluetoothManager, ClassicKeys, CsrkKey,
    LeKeys, LeLongTermKey,
};
use crate::log;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const BLUETOOTH_LIB_PATH: &str = "/var/lib/bluetooth";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct LinuxBluetoothManager { pending: bool }

fn reverse_irk(key: &str) -> Result<String, Box<dyn Error>> {
    validate_bluetooth_key(key, "IRK")?;
    let mut bytes = hex::decode(key)?;
    bytes.reverse();
    Ok(hex::encode_upper(bytes))
}

fn linux_address_type(value: &str) -> Result<&str, Box<dyn Error>> {
    match value {
        "public" => Ok("public"),
        "random" | "static" => Ok("static"),
        _ => Err("Unsupported LE identity address type".into()),
    }
}

fn missing_record_reason(device: &BluetoothDevice) -> Option<String> {
    let validate = || -> Result<(), Box<dyn Error>> {
        if !crate::bluetooth::is_valid_mac_hex(&device.mac_address.replace(':', "")) {
            return Err("invalid device identity".into());
        }
        if let Some(classic) = &device.classic { validate_bluetooth_key(&classic.link_key, "LinkKey")?; }
        if let Some(le) = &device.le {
            let address_type = le.address_type.as_deref().ok_or("missing LE address type")?;
            linux_address_type(address_type)?;
            if address_type != "public" {
                let first = u8::from_str_radix(&device.mac_address[..2], 16)?;
                if first & 0xc0 != 0xc0 { return Err("temporary LE address is not an identity".into()); }
            }
            if le.ltk.is_none() && le.peripheral_ltk.is_none() { return Err("no LE bonding key".into()); }
            for key in [&le.ltk, &le.peripheral_ltk].into_iter().flatten() {
                validate_bluetooth_key(&key.key, "LTK")?;
                if !matches!(key.authenticated, Some(0..=3)) || !matches!(key.enc_size, Some(7..=16))
                    || key.ediv.is_none() || key.rand.is_none() {
                    return Err("incomplete LE security metadata".into());
                }
            }
            if let Some(irk) = &le.irk {
                validate_bluetooth_key(irk, "IRK")?;
                if le.irk_encoding.as_deref() != Some("windows") {
                    return Err("legacy IRK byte order is unverified; export from the updated source OS".into());
                }
            }
        }
        if device.classic.is_none() && device.le.is_none() { return Err("no bonding keys".into()); }
        Ok(())
    };
    validate().err().map(|e| e.to_string())
}

impl LinuxBluetoothManager {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        Ok(Self { pending: false })
    }

    fn start_bluetooth_service(context: &str) -> Result<(), Box<dyn Error>> {
        let status = Command::new("systemctl")
            .args(["start", "bluetooth"])
            .status()
            .map_err(|error| format!("{}: could not run systemctl: {}", context, error))?;
        if !status.success() {
            return Err(format!(
                "{}: systemctl start bluetooth exited with {}",
                context, status
            )
            .into());
        }
        Ok(())
    }

    /// Recover from a previous process dying after it stopped bluetoothd for
    /// an atomic import but before the in-memory batch could be activated.
    pub fn ensure_bluetooth_running() -> Result<(), Box<dyn Error>> {
        Self::start_bluetooth_service("Bluetooth startup reconciliation failed")
    }

    fn get_adapter_info_path(adapter_mac: &str) -> PathBuf {
        PathBuf::from(BLUETOOTH_LIB_PATH).join(normalize_mac(adapter_mac))
    }

    fn get_device_info_path(adapter_mac: &str, device_mac: &str) -> PathBuf {
        Self::get_adapter_info_path(adapter_mac)
            .join(normalize_mac(device_mac))
            .join("info")
    }

    /// Parse the info file and extract all keys (Classic and LE)
    fn read_device_keys(
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<BluetoothDevice, Box<dyn Error>> {
        let info_path = Self::get_device_info_path(adapter_mac, device_mac);
        let content = fs::read_to_string(&info_path)
            .map_err(|e| format!("Failed to read {}: {}", info_path.display(), e))?;

        Self::parse_device_content(&content, device_mac)
    }

    fn parse_device_content(content: &str, device_mac: &str) -> Result<BluetoothDevice, Box<dyn Error>> {
        // Parse INI-like format into sections
        let sections = Self::parse_info_file(content);

        let mut device = BluetoothDevice {
            mac_address: normalize_mac(device_mac),
            name: None,
            pending_deletion: None,
            classic: None,
            le: None,
        };

        // Parse Classic LinkKey
        if let Some(link_key_section) = sections.get("LinkKey") {
            if let Some(key) = link_key_section.get("Key") {
                // Validate LinkKey length
                if let Err(e) = validate_bluetooth_key(key, "LinkKey") {
                    log!(
                        "[BlueVein] Warning: Invalid LinkKey for device {}: {}",
                        device_mac,
                        e
                    );
                } else {
                    let key_type = link_key_section
                        .get("Type")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(4);
                    let pin_length = link_key_section
                        .get("PINLength")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);

                    device.classic = Some(ClassicKeys {
                        link_key: key.clone(),
                        key_type,
                        pin_length,
                    });
                }
            }
        }

        // Parse LE keys
        let mut le_keys = LeKeys::default();
        let mut has_le = false;

        // Parse LongTermKey (Central)
        if let Some(ltk_section) = sections.get("LongTermKey") {
            if let Some(key) = ltk_section.get("Key") {
                // Validate LTK length
                if let Err(e) = validate_bluetooth_key(key, "LTK") {
                    log!(
                        "[BlueVein] Warning: Invalid LTK for device {}: {}",
                        device_mac,
                        e
                    );
                } else {
                    le_keys.ltk = Some(LeLongTermKey {
                        key: key.clone(),
                        authenticated: ltk_section
                            .get("Authenticated")
                            .and_then(|v| v.parse().ok()),
                        enc_size: ltk_section.get("EncSize").and_then(|v| v.parse().ok()),
                        ediv: ltk_section.get("EDiv").and_then(|v| v.parse().ok()),
                        rand: ltk_section.get("Rand").and_then(|v| v.parse().ok()),
                    });
                    has_le = true;
                }
            }
        }

        // Parse PeripheralLongTermKey
        if let Some(pltk_section) = sections.get("PeripheralLongTermKey") {
            if let Some(key) = pltk_section.get("Key") {
                // Validate Peripheral LTK length
                if let Err(e) = validate_bluetooth_key(key, "PeripheralLTK") {
                    log!(
                        "[BlueVein] Warning: Invalid PeripheralLTK for device {}: {}",
                        device_mac,
                        e
                    );
                } else {
                    le_keys.peripheral_ltk = Some(LeLongTermKey {
                        key: key.clone(),
                        authenticated: pltk_section
                            .get("Authenticated")
                            .and_then(|v| v.parse().ok()),
                        enc_size: pltk_section.get("EncSize").and_then(|v| v.parse().ok()),
                        ediv: pltk_section.get("EDiv").and_then(|v| v.parse().ok()),
                        rand: pltk_section.get("Rand").and_then(|v| v.parse().ok()),
                    });
                    has_le = true;
                }
            }
        }

        // Parse IdentityResolvingKey
        if let Some(irk_section) = sections.get("IdentityResolvingKey") {
            if let Some(key) = irk_section.get("Key") {
                // Validate IRK length
                if let Err(e) = validate_bluetooth_key(key, "IRK") {
                    log!(
                        "[BlueVein] Warning: Invalid IRK for device {}: {}",
                        device_mac,
                        e
                    );
                } else {
                    le_keys.irk = Some(reverse_irk(key)?);
                    le_keys.irk_encoding = Some("windows".into());
                    has_le = true;
                }
            }
        }

        // Parse LocalSignatureKey
        if let Some(lsk_section) = sections.get("LocalSignatureKey") {
            if let Some(key) = lsk_section.get("Key") {
                // Validate CSRK length
                if let Err(e) = validate_bluetooth_key(key, "CSRK (Local)") {
                    log!(
                        "[BlueVein] Warning: Invalid LocalSignatureKey for device {}: {}",
                        device_mac,
                        e
                    );
                } else {
                    let counter = lsk_section
                        .get("Counter")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let authenticated = lsk_section
                        .get("Authenticated")
                        .map(|v| v.to_lowercase() == "true")
                        .unwrap_or(false);

                    le_keys.csrk_local = Some(CsrkKey {
                        key: key.clone(),
                        counter,
                        authenticated,
                    });
                    has_le = true;
                }
            }
        }

        // Parse RemoteSignatureKey (CSRK)
        if let Some(rsk_section) = sections.get("RemoteSignatureKey") {
            if let Some(key) = rsk_section.get("Key") {
                // Validate CSRK length
                if let Err(e) = validate_bluetooth_key(key, "CSRK (Remote)") {
                    log!(
                        "[BlueVein] Warning: Invalid RemoteSignatureKey for device {}: {}",
                        device_mac,
                        e
                    );
                } else {
                    let counter = rsk_section
                        .get("Counter")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let authenticated = rsk_section
                        .get("Authenticated")
                        .map(|v| v.to_lowercase() == "true")
                        .unwrap_or(false);

                    le_keys.csrk_remote = Some(CsrkKey {
                        key: key.clone(),
                        counter,
                        authenticated,
                    });
                    has_le = true;
                }
            }
        }

        // Parse AddressType from [General] section
        if let Some(general_section) = sections.get("General") {
            // BlueZ only stores Name after the peer reported one. A user alias
            // is the name this system actually displays, so it is the fallback.
            device.name = general_section.get("Name")
                .or_else(|| general_section.get("Alias"))
                .filter(|name| useful_device_name(name, device_mac))
                .cloned();
            if let Some(addr_type) = general_section.get("AddressType") {
                le_keys.address_type = Some(if addr_type == "static" { "random".into() } else { addr_type.clone() });
                has_le = true;
            }
        }

        if has_le {
            device.le = Some(le_keys);
        }

        if !device.has_keys() {
            return Err(format!("No keys found for device {}", device_mac).into());
        }

        Ok(device)
    }

    /// Parse INI-like info file into sections
    fn parse_info_file(content: &str) -> HashMap<String, HashMap<String, String>> {
        let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current_section = String::new();

        for line in content.lines() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Section header
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                current_section = trimmed[1..trimmed.len() - 1].to_string();
                sections
                    .entry(current_section.clone())
                    .or_insert_with(HashMap::new);
                continue;
            }

            // Key=Value pair
            if let Some(pos) = trimmed.find('=') {
                let key = trimmed[..pos].trim().to_string();
                let value = trimmed[pos + 1..].trim().to_string();

                if !current_section.is_empty() {
                    sections
                        .entry(current_section.clone())
                        .or_insert_with(HashMap::new)
                        .insert(key, value);
                }
            }
        }

        sections
    }

    /// Write device info to file (both Classic and LE keys)
    fn write_device_file(
        info_path: &std::path::Path,
        device: &BluetoothDevice,
    ) -> Result<(), Box<dyn Error>> {
        let device_dir = info_path.parent().ok_or("Missing device directory")?;

        // Validate a new bond before creating any on-disk state.
        let creating = !info_path.exists();
        if creating {
            if let Some(reason) = missing_record_reason(device) { return Err(reason.into()); }
        }
        fs::create_dir_all(&device_dir)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&device_dir, fs::Permissions::from_mode(0o700))?;

        // Read existing file if it exists
        let existing_sections = if info_path.exists() {
            let content = fs::read_to_string(&info_path)?;
            Self::parse_info_file(&content)
        } else {
            HashMap::new()
        };

        // Build new sections map
        let mut sections = existing_sections;
        if creating {
            let general = sections.entry("General".into()).or_insert_with(HashMap::new);
            general.insert("Trusted".into(), "true".into());
            general.insert("SupportedTechnologies".into(), match (device.classic.is_some(), device.le.is_some()) {
                (true, true) => "BR/EDR;LE;", (true, false) => "BR/EDR;", _ => "LE;",
            }.into());
        }
        if let Some(name) = device.name.as_ref().filter(|name| useful_device_name(name, &device.mac_address)) {
            let general = sections.entry("General".into()).or_insert_with(HashMap::new);
            general.insert("Name".into(), name.clone());
        }

        // Update Classic LinkKey
        if let Some(classic) = &device.classic {
            // Validate before writing
            validate_bluetooth_key(&classic.link_key, "LinkKey")?;

            let link_key_section = sections
                .entry("LinkKey".to_string())
                .or_insert_with(HashMap::new);
            link_key_section.insert("Key".to_string(), classic.link_key.clone());
            link_key_section.insert("Type".to_string(), classic.key_type.to_string());
            link_key_section.insert("PINLength".to_string(), classic.pin_length.to_string());
        }

        // Update LE keys
        if let Some(le) = &device.le {
            // LongTermKey (Central)
            if let Some(ltk) = &le.ltk {
                // Validate before writing
                validate_bluetooth_key(&ltk.key, "LTK")?;

                let ltk_section = sections
                    .entry("LongTermKey".to_string())
                    .or_insert_with(HashMap::new);
                ltk_section.insert("Key".to_string(), ltk.key.clone());
                // Use authenticated_or_default() to ensure we write 0 if not set
                ltk_section.insert(
                    "Authenticated".to_string(),
                    ltk.authenticated_or_default().to_string(),
                );
                if let Some(enc_size) = ltk.enc_size {
                    ltk_section.insert("EncSize".to_string(), enc_size.to_string());
                }
                if let Some(ediv) = ltk.ediv {
                    ltk_section.insert("EDiv".to_string(), ediv.to_string());
                }
                if let Some(rand) = ltk.rand {
                    ltk_section.insert("Rand".to_string(), rand.to_string());
                }
            }

            // PeripheralLongTermKey
            if let Some(pltk) = &le.peripheral_ltk {
                // Validate before writing
                validate_bluetooth_key(&pltk.key, "PeripheralLTK")?;

                let pltk_section = sections
                    .entry("PeripheralLongTermKey".to_string())
                    .or_insert_with(HashMap::new);
                pltk_section.insert("Key".to_string(), pltk.key.clone());
                pltk_section.insert(
                    "Authenticated".to_string(),
                    pltk.authenticated_or_default().to_string(),
                );
                if let Some(enc_size) = pltk.enc_size {
                    pltk_section.insert("EncSize".to_string(), enc_size.to_string());
                }
                if let Some(ediv) = pltk.ediv {
                    pltk_section.insert("EDiv".to_string(), ediv.to_string());
                }
                if let Some(rand) = pltk.rand {
                    pltk_section.insert("Rand".to_string(), rand.to_string());
                }
            }

            // IdentityResolvingKey
            if let Some(irk) = &le.irk {
                // Validate before writing
                validate_bluetooth_key(irk, "IRK")?;

                let irk_section = sections
                    .entry("IdentityResolvingKey".to_string())
                    .or_insert_with(HashMap::new);
                irk_section.insert("Key".to_string(), match le.irk_encoding.as_deref() {
                    Some("windows") => reverse_irk(irk)?,
                    None => return Err("Legacy IRK encoding must be verified before import".into()),
                    Some(_) => return Err("Unsupported IRK encoding".into()),
                });
            }

            // LocalSignatureKey
            if let Some(csrk_local) = &le.csrk_local {
                // Validate before writing
                validate_bluetooth_key(&csrk_local.key, "CSRK (Local)")?;

                let lsk_section = sections
                    .entry("LocalSignatureKey".to_string())
                    .or_insert_with(HashMap::new);
                lsk_section.insert("Key".to_string(), csrk_local.key.clone());
                lsk_section.insert("Counter".to_string(), csrk_local.counter.to_string());
                lsk_section.insert(
                    "Authenticated".to_string(),
                    csrk_local.authenticated.to_string(),
                );
            }

            // RemoteSignatureKey
            if let Some(csrk_remote) = &le.csrk_remote {
                // Validate before writing
                validate_bluetooth_key(&csrk_remote.key, "CSRK (Remote)")?;

                let rsk_section = sections
                    .entry("RemoteSignatureKey".to_string())
                    .or_insert_with(HashMap::new);
                rsk_section.insert("Key".to_string(), csrk_remote.key.clone());
                rsk_section.insert("Counter".to_string(), csrk_remote.counter.to_string());
                rsk_section.insert(
                    "Authenticated".to_string(),
                    csrk_remote.authenticated.to_string(),
                );
            }

            // AddressType in [General] section
            if let Some(address_type) = &le.address_type {
                let general_section = sections
                    .entry("General".to_string())
                    .or_insert_with(HashMap::new);
                general_section.insert("AddressType".to_string(), linux_address_type(address_type)?.into());
            }
        }

        // Serialize sections back to file
        let mut content = String::new();
        for (section_name, section_data) in sections {
            content.push_str(&format!("[{}]\n", section_name));
            for (key, value) in section_data {
                content.push_str(&format!("{}={}\n", key, value));
            }
            content.push('\n');
        }

        // Publish a complete private record atomically; never expose partial keys.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = device_dir.join(format!(
            ".bluevein-info.{}.{}.{}.tmp",
            std::process::id(),
            nonce,
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        let published = (|| -> Result<(), Box<dyn Error>> {
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &info_path)?;
            fs::File::open(&device_dir)?.sync_all()?;
            Ok(())
        })();
        if published.is_err() { let _ = fs::remove_file(&temporary); }
        published?;

        Ok(())
    }


}

impl BluetoothManager for LinuxBluetoothManager {
    fn platform_id(&self) -> &'static str { "linux" }
    fn needs_update(&self, current: &BluetoothDevice, desired: &BluetoothDevice) -> bool {
        let mut current_keys = current.clone();
        let mut desired_keys = desired.clone();
        current_keys.name = None;
        desired_keys.name = None;
        current_keys.pending_deletion = None;
        desired_keys.pending_deletion = None;
        current_keys != desired_keys ||
            (desired.name.as_ref().is_some_and(|name| useful_device_name(name, &desired.mac_address))
                && current.name != desired.name)
    }
    fn migrate_shared_config(&self, config: &mut crate::config::BlueVeinConfig) -> Result<(), Box<dyn Error>> {
        // Tag only byte order already established by the local canonical key.
        // Never guess the origin of a different untagged shared IRK.
        for adapter in self.get_adapters()? {
            for local in self.get_devices(&adapter)? {
                let Some(shared) = config.adapters.get_mut(&adapter).and_then(|a| a.devices.get_mut(&local.mac_address)) else { continue; };
                if let (Some(a), Some(b)) = (&local.le, &mut shared.le) {
                    if b.irk_encoding.is_none() && b.irk.is_some() && a.irk.as_ref().zip(b.irk.as_ref()).is_some_and(|(x,y)| x.eq_ignore_ascii_case(y)) {
                        b.irk_encoding = Some("windows".into());
                    }
                    if b.address_type.as_deref() == Some("static") { b.address_type = Some("random".into()); }
                }
            }
        }
        Ok(())
    }

    fn update_reason(&self, _current: &BluetoothDevice, desired: &BluetoothDevice) -> Option<String> {
        if let Some(le) = &desired.le {
            if le.irk.is_some() && le.irk_encoding.as_deref() != Some("windows") {
                return Some("unverified legacy IRK encoding; export from updated source OS first".into());
            }
        }
        None
    }

    fn import_missing_reason(&self, device: &BluetoothDevice) -> Option<String> {
        missing_record_reason(device)
    }

    fn apply_pending(&mut self) -> Result<(), Box<dyn Error>> {
        if self.pending {
            Self::start_bluetooth_service("Bluetooth batch activation failed")?;
            self.pending = false;
        }
        Ok(())
    }

    fn get_adapters(&self) -> Result<Vec<String>, Box<dyn Error>> {
        let mut adapters = Vec::new();

        if !PathBuf::from(BLUETOOTH_LIB_PATH).exists() {
            return Ok(adapters);
        }

        for entry in fs::read_dir(BLUETOOTH_LIB_PATH)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            // Check if it looks like a MAC address
            if name.contains(':') && name.len() == 17 && entry.file_type()?.is_dir() {
                adapters.push(normalize_mac(&name));
            }
        }

        Ok(adapters)
    }

    fn get_devices(&self, adapter_mac: &str) -> Result<Vec<BluetoothDevice>, Box<dyn Error>> {
        let adapter_path = Self::get_adapter_info_path(adapter_mac);
        let mut devices = Vec::new();

        if !adapter_path.exists() {
            return Ok(devices);
        }

        for entry in fs::read_dir(&adapter_path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }

            let device_mac = entry.file_name().to_string_lossy().to_string();

            // Check if it looks like a MAC address
            if device_mac.contains(':') && device_mac.len() == 17 {
                if let Ok(device) = Self::read_device_keys(adapter_mac, &device_mac) {
                    devices.push(device);
                }
            }
        }

        Ok(devices)
    }

    fn get_device(
        &self,
        adapter_mac: &str,
        device_mac: &str,
    ) -> Result<BluetoothDevice, Box<dyn Error>> {
        Self::read_device_keys(adapter_mac, device_mac)
    }

    fn set_device(
        &mut self,
        adapter_mac: &str,
        device: &BluetoothDevice,
    ) -> Result<(), Box<dyn Error>> {
        if !self.pending {
            // Stop once before writing: bluetoothd must not flush stale in-memory
            // records over the imported keys when it exits.
            let status = Command::new("systemctl").args(["stop", "bluetooth"]).status()?;
            if !status.success() { return Err("Could not stop Bluetooth before import".into()); }
            self.pending = true;
        }
        Self::write_device_file(&Self::get_device_info_path(adapter_mac, &device.mac_address), device)?;
        Ok(())
    }

    fn remove_device(&mut self, adapter_mac: &str, device_mac: &str) -> Result<(), Box<dyn Error>> {
        // Ask the running daemon to forget the bond. Removing its info file
        // behind bluetoothd's back lets it recreate the old bond on shutdown.
        let tree = Command::new("busctl").args(["tree", "org.bluez"]).output()?;
        if !tree.status.success() { return Err("Cannot enumerate BlueZ adapters".into()); }
        let mut adapter_path = None;
        for path in String::from_utf8(tree.stdout)?.lines().filter_map(|line| line.split_whitespace().last()) {
            if !path.starts_with("/org/bluez/hci") || path["/org/bluez/".len()..].contains('/') { continue; }
            let value = Command::new("busctl")
                .args(["get-property", "org.bluez", path, "org.bluez.Adapter1", "Address"])
                .output()?;
            if value.status.success() && String::from_utf8(value.stdout)?.contains(&normalize_mac(adapter_mac)) {
                adapter_path = Some(path.to_string());
                break;
            }
        }
        let adapter_path = adapter_path.ok_or("Bluetooth adapter is not exported by BlueZ")?;
        let device_path = format!("{}/dev_{}", adapter_path, normalize_mac(device_mac).replace(':', "_"));
        // BlueZ can expose the adapter before it is ready to process removals
        // during boot. Keep the deletion marker while retrying this one D-Bus
        // call; restarting the whole service would rerun unrelated imports.
        for attempt in 0..7 {
            let result = Command::new("busctl").args([
                "call", "org.bluez", &adapter_path, "org.bluez.Adapter1", "RemoveDevice", "o", &device_path,
            ]).output()?;
            if result.status.success() { return Ok(()); }
            if !Self::get_device_info_path(adapter_mac, device_mac).exists() { return Ok(()); }
            let error = String::from_utf8_lossy(&result.stderr);
            let transient = error.contains("Resource Not Ready") || error.contains("NotReady");
            if !transient || attempt == 6 {
                return Err(format!("BlueZ refused to remove {}: {}", device_mac, error.trim()).into());
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        unreachable!()
    }
}

#[cfg(test)]
mod general_import_tests {
    use super::*;
    fn bond() -> BluetoothDevice {
        BluetoothDevice { mac_address: "D0:11:22:33:44:55".into(), name: None, pending_deletion: None, classic: None,
            le: Some(LeKeys { address_type: Some("random".into()),
                irk: Some("000102030405060708090A0B0C0D0E0F".into()), irk_encoding: Some("windows".into()),
                ltk: Some(LeLongTermKey { key: "AA".repeat(16), authenticated: Some(2), enc_size: Some(16), ediv: Some(0), rand: Some(0) }),
                ..Default::default() }) }
    }
    #[test]
    fn real_bluez_files_round_trip_keys_types_and_preserve_unrelated_settings() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("bluevein-bonds-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        for (i, auth) in [0, 1, 2, 3].into_iter().enumerate() {
            let mut d = bond();
            d.mac_address = format!("D0:11:22:33:44:{i:02X}");
            d.le.as_mut().unwrap().ltk.as_mut().unwrap().authenticated = Some(auth);
            if auth < 2 {
                let key = d.le.as_mut().unwrap().ltk.as_mut().unwrap(); key.ediv = Some(456); key.rand = Some(987654321);
            }
            let path = root.join(&d.mac_address).join("info");
            LinuxBluetoothManager::write_device_file(&path, &d).unwrap();
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains("AddressType=static"));
            assert!(content.contains("Trusted=true"));
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(LinuxBluetoothManager::parse_device_content(&content, &d.mac_address).unwrap(), d);
            fs::write(&path, format!("{content}\n[Custom]\nKeep=value\n")).unwrap();
            LinuxBluetoothManager::write_device_file(&path, &d).unwrap();
            assert!(fs::read_to_string(&path).unwrap().contains("Keep=value"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_legacy_temporary_file_does_not_block_atomic_publish() {
        let root = std::env::temp_dir().join(format!(
            "bluevein-stale-temp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("info");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join(".bluevein-info.tmp");
        fs::write(&stale, "orphaned partial data").unwrap();

        let device = bond();
        LinuxBluetoothManager::write_device_file(&path, &device).unwrap();

        assert!(path.exists());
        assert!(stale.exists());
        assert_eq!(
            LinuxBluetoothManager::parse_device_content(
                &fs::read_to_string(&path).unwrap(),
                &device.mac_address
            )
            .unwrap(),
            device
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_name_updates_bluez_name_without_erasing_local_alias() {
        let root = std::env::temp_dir().join(format!("bluevein-name-{}", std::process::id()));
        let path = root.join("info");
        let mut device = bond();
        device.name = Some("MX Keys".into());
        LinuxBluetoothManager::write_device_file(&path, &device).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(LinuxBluetoothManager::parse_device_content(&content, &device.mac_address).unwrap().name, device.name);
        fs::write(&path, content.replace("Name=MX Keys", "Name=Old MX\nAlias=Desk keyboard")).unwrap();
        assert!(LinuxBluetoothManager::new().unwrap().needs_update(
            &LinuxBluetoothManager::parse_device_content(&fs::read_to_string(&path).unwrap(), &device.mac_address).unwrap(),
            &device));
        LinuxBluetoothManager::write_device_file(&path, &device).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Name=MX Keys"));
        assert!(content.contains("Alias=Desk keyboard"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn user_alias_is_exported_when_bluez_never_stored_a_name() {
        let root = std::env::temp_dir().join(format!("bluevein-alias-{}", std::process::id()));
        let path = root.join("info");
        let device = bond();
        LinuxBluetoothManager::write_device_file(&path, &device).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("Name="));
        let aliased = content.replace("[General]\n", "[General]\nAlias=Desk keyboard\n");
        fs::write(&path, &aliased).unwrap();
        let parsed = LinuxBluetoothManager::parse_device_content(&aliased, &device.mac_address).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("Desk keyboard"));
        // A reported name still wins over the local alias.
        let named = aliased.replace("[General]\n", "[General]\nName=MX Keys\n");
        assert_eq!(LinuxBluetoothManager::parse_device_content(&named, &device.mac_address).unwrap().name.as_deref(), Some("MX Keys"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn arbitrary_devices_are_importable_without_mac_allowlist() {
        for mac in ["D0:11:22:33:44:55", "F1:AA:BB:CC:DD:EE", "C2:00:00:00:00:01"] {
            let mut d = bond(); d.mac_address = mac.into(); assert!(missing_record_reason(&d).is_none());
        }
        let mut d = bond(); d.mac_address = "00:11:22:33:44:55".into();
        assert!(missing_record_reason(&d).is_some());
        d.le.as_mut().unwrap().address_type = Some("public".into());
        assert!(missing_record_reason(&d).is_none());
    }
    #[test]
    fn incomplete_or_ambiguous_security_data_is_not_guessed() {
        let mut d = bond(); d.le.as_mut().unwrap().ltk.as_mut().unwrap().authenticated = None;
        assert!(missing_record_reason(&d).unwrap().contains("metadata"));
        let mut d = bond(); d.le.as_mut().unwrap().irk_encoding = None;
        assert!(missing_record_reason(&d).unwrap().contains("byte order"));
        let mut d = bond(); d.le.as_mut().unwrap().ltk = None;
        assert!(missing_record_reason(&d).unwrap().contains("bonding key"));
    }
    #[test]
    fn address_type_and_irk_use_platform_format_not_device_identity() {
        assert_eq!(linux_address_type("random").unwrap(), "static");
        assert_eq!(linux_address_type("static").unwrap(), "static");
        assert_eq!(linux_address_type("public").unwrap(), "public");
        assert!(linux_address_type("unknown").is_err());
        let key = "000102030405060708090A0B0C0D0E0F";
        assert_eq!(reverse_irk(key).unwrap(), "0F0E0D0C0B0A09080706050403020100");
        assert_eq!(reverse_irk(&reverse_irk(key).unwrap()).unwrap(), key);
    }
}

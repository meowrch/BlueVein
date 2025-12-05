use crate::bluetooth::BluetoothDevice;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bluetooth device configuration for an adapter
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceConfig {
    /// Paired devices: MAC address -> Device info (Classic and/or LE keys)
    pub devices: HashMap<String, BluetoothDevice>,
}

/// Root configuration structure
/// Key: Adapter MAC address
/// Value: Device configuration for that adapter
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BlueVeinConfig {
    #[serde(flatten)]
    pub adapters: HashMap<String, DeviceConfig>,
}

impl BlueVeinConfig {
    /// Create a new empty configuration
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse configuration from JSON string
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Serialize configuration to JSON string
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Get devices for a specific adapter
    pub fn get_adapter_devices(&self, adapter_mac: &str) -> Option<&HashMap<String, BluetoothDevice>> {
        self.adapters.get(adapter_mac).map(|config| &config.devices)
    }

    /// Set devices for a specific adapter
    pub fn set_adapter_devices(&mut self, adapter_mac: String, devices: HashMap<String, BluetoothDevice>) {
        self.adapters.insert(adapter_mac, DeviceConfig { devices });
    }

    /// Add or update a single device for an adapter
    pub fn update_device(&mut self, adapter_mac: String, device: BluetoothDevice) {
        let device_mac = device.mac_address.clone();
        self.adapters
            .entry(adapter_mac)
            .or_insert_with(|| DeviceConfig {
                devices: HashMap::new(),
            })
            .devices
            .insert(device_mac, device);
    }

    /// Get a specific device
    pub fn get_device(&self, adapter_mac: &str, device_mac: &str) -> Option<&BluetoothDevice> {
        self.get_adapter_devices(adapter_mac)
            .and_then(|devices| devices.get(device_mac))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::BluetoothDevice;

    #[test]
    fn test_config_serialization() {
        let mut config = BlueVeinConfig::new();
        let mut devices = HashMap::new();
        devices.insert(
            "AA:BB:CC:DD:EE:FF".to_string(),
            BluetoothDevice::classic(
                "AA:BB:CC:DD:EE:FF".to_string(),
                "0123456789ABCDEF".to_string(),
            ),
        );

        config.set_adapter_devices("00:11:22:33:44:55".to_string(), devices);

        let json = config.to_json().unwrap();
        let parsed = BlueVeinConfig::from_json(&json).unwrap();

        assert_eq!(config, parsed);
    }

    #[test]
    fn test_update_device() {
        let mut config = BlueVeinConfig::new();
        let device = BluetoothDevice::classic(
            "AA:BB:CC:DD:EE:FF".to_string(),
            "KEY123".to_string(),
        );
        config.update_device("00:11:22:33:44:55".to_string(), device);

        let stored = config.get_device("00:11:22:33:44:55", "AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(stored.classic.as_ref().unwrap().link_key, "KEY123");
    }
}

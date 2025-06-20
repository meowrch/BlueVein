use anyhow::Result;
use shared::{Config, get_adapter_mac, read_link_key, update_common_file};
use dbus::{Message, arg};
use dbus::blocking::Connection;
use std::time::Duration;
use log::{info, error, warn};

const CONFIG_PATH: &str = "/etc/bluevein.conf";

fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();
    
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
    
    let conn = Connection::new_system()?;
    let rule = dbus::message::MatchRule::new()
        .with_interface("org.freedesktop.DBus.ObjectManager")
        .with_member("InterfacesAdded");
    
    let efi_path = config.efi_path.clone();
    let adapter_mac_clone = adapter_mac.clone();

    let proxy = conn.with_proxy("org.bluez", "/", Duration::from_secs(5));
    let (objects,): (std::collections::HashMap<dbus::Path, std::collections::HashMap<String, std::collections::HashMap<String, arg::Variant<Box<dyn arg::RefArg>>>>>,) =
        proxy.method_call("org.freedesktop.DBus.ObjectManager", "GetManagedObjects", ())?;

    for (path, interfaces) in objects {
        if let Some(props) = interfaces.get("org.bluez.Device1") {
            if let Some(paired) = props.get("Paired").and_then(|v| arg::cast::<bool>(&*v.0)) {
                if *paired {
                    if let Some(addr) = props.get("Address").and_then(|v| v.0.as_str()) {
                        info!("(Startup) Device paired: {}", addr);
                        if let Some(key) = read_link_key(&adapter_mac, addr) {
                            info!("Found key for {}: {}", addr, key);
                            if let Err(e) = update_common_file(&efi_path, &adapter_mac, addr, &key) {
                                error!("Failed to update common file: {}", e);
                            } else {
                                info!("Key updated for {}", addr);
                            }
                        } else {
                            warn!("No link key found for {}", addr);
                        }
                    }
                }
            }
        }
    }
    
    conn.add_match(rule, move |_: (dbus::Path, std::collections::HashMap<String, std::collections::HashMap<String, arg::Variant<Box<dyn arg::RefArg>>>>), 
                   _: &Connection, 
                   msg: &Message| {
        info!("Received D-Bus event: {:?}", msg);

        let (path, interfaces) = msg.get2::<
            dbus::Path, 
            std::collections::HashMap<String, std::collections::HashMap<String, arg::Variant<Box<dyn arg::RefArg>>>>
        >();
                
        if let (Some(path), Some(interfaces)) = (path, interfaces) {
            if path.to_string().starts_with("/org/bluez/") {
                if let Some(props) = interfaces.get("org.bluez.Device1") {
                    if let Some(paired) = props.get("Paired").and_then(|v| arg::cast::<bool>(&*v.0))  {
                        if *paired {
                            if let Some(addr) = props.get("Address").and_then(|v| v.0.as_str()) {
                                info!("Device paired: {}", addr);
                                if let Some(key) = read_link_key(&adapter_mac_clone, addr) {
                                    if let Err(e) = update_common_file(&efi_path, &adapter_mac_clone, addr, &key) {
                                        error!("Failed to update common file: {}", e);
                                    } else {
                                        info!("Key updated for {}", addr);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        true
    })?;
    
    info!("Service started. Monitoring Bluetooth events...");
    loop {
        conn.process(Duration::from_secs(1))?;
    }
}
mod bluetooth;
mod config;
mod efi;
mod logger;
mod sync;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
mod linux;

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "windows")]
    return windows::run();

    #[cfg(target_os = "linux")]
    return linux::run();
}

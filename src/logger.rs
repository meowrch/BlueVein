//! Cross-platform logging module
//!
//! On Windows: logs to C:\ProgramData\BlueVein\bluevein.log
//! On Linux: logs to stdout (captured by systemd)

#[cfg(target_os = "windows")]
use std::fs::{create_dir_all, OpenOptions};
#[cfg(target_os = "windows")]
use std::io::Write;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::sync::Mutex;

#[cfg(target_os = "windows")]
static LOGGER: once_cell::sync::Lazy<Mutex<Option<std::fs::File>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(init_logger()));

#[cfg(target_os = "windows")]
fn init_logger() -> Option<std::fs::File> {
    // Use C:\ProgramData\BlueVein for logs (writable by SYSTEM)
    let log_dir = PathBuf::from("C:\\ProgramData\\BlueVein");

    if let Err(e) = create_dir_all(&log_dir) {
        eprintln!("[BlueVein] Failed to create log directory: {}", e);
        return None;
    }

    let log_path = log_dir.join("bluevein.log");

    match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => {
            eprintln!("[BlueVein] Logging to: {:?}", log_path);
            Some(file)
        }
        Err(e) => {
            eprintln!("[BlueVein] Failed to open log file: {}", e);
            None
        }
    }
}

/// Log a message (cross-platform)
#[cfg(target_os = "windows")]
pub fn log(msg: &str) {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_line = format!("[{}] {}", timestamp, msg);

    // Print to console (for standalone mode)
    println!("{}", log_line);

    // Write to file
    if let Ok(mut logger) = LOGGER.lock() {
        if let Some(ref mut file) = *logger {
            let _ = writeln!(file, "{}", log_line);
            let _ = file.flush();
        }
    }
}

#[cfg(target_os = "linux")]
pub fn log(msg: &str) {
    // On Linux, just print to stdout (systemd will capture it)
    println!("{}", msg);
}

/// Convenience macro for formatted logging
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::logger::log(&format!($($arg)*));
    }};
}

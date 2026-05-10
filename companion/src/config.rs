use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Espressif USB-Serial-JTAG VID/PID for ESP32-S3.
const ESPRESSIF_VID: u16 = 0x303A;
const ESP32S3_JTAG_PID: u16 = 0x1001;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// COM port override. Empty string = auto-detect by USB VID/PID.
    #[serde(default)]
    pub port: String,
    /// Volume change per encoder click, in percent (1–10).
    #[serde(default = "default_sensitivity")]
    pub sensitivity_pct: u8,
    /// Register this executable for Windows auto-start on login.
    #[serde(default)]
    pub autostart: bool,
    /// Active backlight brightness (1–100).
    #[serde(default = "default_backlight_pct")]
    pub backlight_pct: u8,
    /// Idle seconds before the backlight dims. 0 disables.
    #[serde(default = "default_dim_after_secs")]
    pub backlight_dim_after_secs: u16,
    /// Additional idle seconds (after dimming) before the backlight switches
    /// off. 0 disables — the screen stays at the dim level forever.
    #[serde(default = "default_off_after_secs")]
    pub backlight_off_after_secs: u16,
    /// Transport mode: "usb", "wifi", or "auto" (try WiFi, fall back to USB).
    #[serde(default = "default_connection_mode")]
    pub connection_mode: String,
    /// Device IP saved after successful WiFi provisioning.
    #[serde(default)]
    pub wifi_device_ip: String,
    /// TCP port the device listens on (default 9000).
    #[serde(default = "default_wifi_port")]
    pub wifi_port: u16,
}

fn default_sensitivity() -> u8 {
    2
}
fn default_connection_mode() -> String {
    "usb".to_string()
}
fn default_wifi_port() -> u16 {
    9000
}
fn default_backlight_pct() -> u8 {
    100
}
fn default_dim_after_secs() -> u16 {
    30
}
fn default_off_after_secs() -> u16 {
    90
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: String::new(),
            sensitivity_pct: 2,
            autostart: false,
            backlight_pct: default_backlight_pct(),
            backlight_dim_after_secs: default_dim_after_secs(),
            backlight_off_after_secs: default_off_after_secs(),
            connection_mode: default_connection_mode(),
            wifi_device_ip: String::new(),
            wifi_port: default_wifi_port(),
        }
    }
}

fn config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("RustyEspKnob");
    p.push("config.toml");
    p
}

pub fn load() -> Config {
    let path = config_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(path, s);
    }
}

/// Load config, update `wifi_device_ip`, and save.
pub fn save_wifi_ip(ip: &str) {
    let mut cfg = load();
    cfg.wifi_device_ip = ip.to_string();
    save(&cfg);
}

/// Scan available serial ports and return the first one with the
/// Espressif USB-Serial-JTAG VID/PID (the ESP32-S3 built-in interface).
pub fn find_device_port() -> Option<String> {
    let ports = serialport::available_ports().ok()?;
    for port in ports {
        if let serialport::SerialPortType::UsbPort(info) = port.port_type {
            if info.vid == ESPRESSIF_VID && info.pid == ESP32S3_JTAG_PID {
                return Some(port.port_name);
            }
        }
    }
    None
}

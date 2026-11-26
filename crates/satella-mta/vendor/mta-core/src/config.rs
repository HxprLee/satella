//! Application configuration and persistence
//!
//! Provides storage and retrieval of settings such as the device name and vendor ID.

use log::debug;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Vendor ID enum
///
/// List of vendor IDs compatible with CatShare
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BrandId {
    #[default]
    Unknown = 0,
    Windows = 4,
    Oppo = 10,
    Realme = 11,
    Vivo = 20,
    Xiaomi = 30, // 30-39 range (excluding 32)
    BlackShark = 32,
    OnePlus = 41,   // 41-45
    Meizu = 50,     // 50-59
    Nubia = 60,     // 60-69
    Samsung = 70,   // 70-75
    ZTE = 80,       // 80-89
    Smartisan = 90, // 90-95
    Lenovo = 100,   // 100-109
    Motorola = 110, // 110-119
    NIO = 120,      // 120-129
    Honor = 140,    // 140-149
    ROG = 160,      // 0xA0 (-96)
    Asus = 161,     // 0xA1..0xA9 (-95..-87) (Generic Asus in range)
    Hisense = 170,  // 0xAA..0xB3 (-86..-77)
    // Custom ID for Linux devices
    Linux = 200,
}

impl BrandId {
    /// Get the list of all supported vendors
    pub fn all() -> &'static [BrandId] {
        &[
            BrandId::Xiaomi,
            BrandId::BlackShark,
            BrandId::Vivo,
            BrandId::Oppo,
            BrandId::Realme,
            BrandId::OnePlus,
            BrandId::Honor,
            BrandId::Meizu,
            BrandId::Samsung,
            BrandId::Lenovo,
            BrandId::Motorola,
            BrandId::ZTE,
            BrandId::Nubia,
            BrandId::Smartisan,
            BrandId::Asus,
            BrandId::ROG,
            BrandId::Hisense,
            BrandId::NIO,
            BrandId::Windows,
            BrandId::Linux,
            BrandId::Unknown,
        ]
    }

    /// Get the vendor name
    pub fn name(&self) -> &'static str {
        match self {
            BrandId::Unknown => "Unknown",
            BrandId::Windows => "Windows",
            BrandId::Oppo => "OPPO",
            BrandId::Realme => "realme",
            BrandId::Vivo => "vivo",
            BrandId::Xiaomi => "Xiaomi",
            BrandId::BlackShark => "Black Shark",
            BrandId::OnePlus => "OnePlus",
            BrandId::Meizu => "Meizu",
            BrandId::Nubia => "Nubia",
            BrandId::Samsung => "Samsung",
            BrandId::ZTE => "ZTE",
            BrandId::Smartisan => "Smartisan",
            BrandId::Lenovo => "Lenovo",
            BrandId::Motorola => "Motorola",
            BrandId::NIO => "NIO",
            BrandId::Honor => "Honor",
            BrandId::Asus => "Asus",
            BrandId::ROG => "ROG",
            BrandId::Hisense => "Hisense",
            BrandId::Linux => "Linux",
        }
    }

    /// Create from an ID value
    pub fn from_id(id: u8) -> Self {
        match id {
            4 | 21 => BrandId::Windows,
            10..=19 => {
                if id == 11 {
                    BrandId::Realme
                } else {
                    BrandId::Oppo
                }
            }
            20..=29 => BrandId::Vivo,
            30..=39 => {
                if id == 32 {
                    BrandId::BlackShark
                } else {
                    BrandId::Xiaomi
                }
            }
            41..=45 => BrandId::OnePlus,
            50..=59 => BrandId::Meizu,
            60..=69 => BrandId::Nubia,
            70..=75 => BrandId::Samsung,
            80..=89 => BrandId::ZTE,
            90..=95 => BrandId::Smartisan,
            100..=109 => BrandId::Lenovo,
            110..=119 => BrandId::Motorola,
            120..=129 => BrandId::NIO,
            140..=149 => BrandId::Honor,
            // Negative ranges mapped to u8 (e.g. -96 is 0xA0 = 160)
            160..=169 => {
                // -96 to -87 (0xA0 to 0xA9)
                if id == 160 {
                    BrandId::ROG
                } else {
                    BrandId::Asus
                }
            }
            170..=179 => BrandId::Hisense, // -86 to -77 (0xAA to 0xB3)
            200 => BrandId::Linux,
            _ => BrandId::Unknown,
        }
    }

    /// Get the ID value
    pub fn id(&self) -> u8 {
        *self as u8
    }
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// Device name (shown during scanning)
    pub device_name: String,
    /// Vendor ID
    pub brand_id: BrandId,
    /// Whether 5GHz WiFi is supported
    pub supports_5ghz: bool,
    /// WiFi interface name
    pub wifi_interface: String,
    /// Download directory
    pub download_dir: PathBuf,
    /// Whether to automatically accept transfers
    pub auto_accept: bool,
    /// Verbose logging mode
    pub verbose: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            device_name: get_default_device_name(),
            brand_id: BrandId::Xiaomi,
            supports_5ghz: true,
            wifi_interface: "wlan0".to_string(),
            download_dir: dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
            auto_accept: false,
            verbose: false,
        }
    }
}

impl AppSettings {
    /// Get the configuration file path
    fn config_path() -> PathBuf {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cattysend");
        config_dir.join("settings.toml")
    }

    /// Load settings (use defaults if the file does not exist)
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => match toml::from_str(&content) {
                    Ok(settings) => {
                        debug!("Loaded settings from {:?}", path);
                        return settings;
                    }
                    Err(e) => {
                        log::warn!("Failed to parse settings: {}, using defaults", e);
                    }
                },
                Err(e) => {
                    log::warn!("Failed to read settings file: {}, using defaults", e);
                }
            }
        }
        Self::default()
    }

    /// Save settings
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        debug!("Saved settings to {:?}", path);
        Ok(())
    }

    /// Get the capability UUID used for broadcasting
    ///
    /// Format: 0000XXYY-0000-1000-8000-00805f9b34fb
    /// - XX = 5GHz flag (0x01 = supported, 0x00 = not supported)
    /// - YY = vendor ID
    pub fn capability_uuid(&self) -> uuid::Uuid {
        let flag_5ghz: u8 = if self.supports_5ghz { 0x01 } else { 0x00 };
        let brand = self.brand_id.id();
        // Build the UUID: 0000XXYY-0000-1000-8000-00805f9b34fb
        let high = (flag_5ghz as u16) << 8 | (brand as u16);
        uuid::Uuid::from_u128(((high as u128) << 96) | (0x0000_1000_8000_0080_5f9b_34fb_u128))
    }
}

/// Get the default device name (hostname)
fn get_default_device_name() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "Cattysend".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_brand_id() {
        assert_eq!(BrandId::Xiaomi.id(), 30);
        assert_eq!(BrandId::from_id(30).name(), "Xiaomi");
    }

    #[test]
    fn test_brand_id_all_coverage() {
        let all = BrandId::all();
        // Check that essential brands are present
        assert!(all.contains(&BrandId::Xiaomi));
        assert!(all.contains(&BrandId::Oppo));
        assert!(all.contains(&BrandId::Vivo));
        assert!(all.contains(&BrandId::Honor));
        assert!(all.contains(&BrandId::Windows));

        // Ensure uniqueness
        let mut sorted = all.to_vec();
        sorted.sort_by_key(|b| b.id());
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            all.len(),
            "BrandId::all() contains duplicates"
        );
    }

    #[test]
    fn test_capability_uuid() {
        let settings = AppSettings {
            supports_5ghz: true,
            brand_id: BrandId::Xiaomi, // 30 = 0x1E
            ..Default::default()
        };

        let uuid = settings.capability_uuid();
        let uuid_str = uuid.to_string();
        // Should be 0000011e-0000-1000-8000-00805f9b34fb
        assert!(uuid_str.starts_with("0000011e"), "UUID: {}", uuid_str);
        assert!(uuid_str.ends_with("00805f9b34fb"), "UUID: {}", uuid_str);
    }

    #[test]
    fn test_default_settings() {
        let settings = AppSettings::default();
        // Default to Xiaomi to ensure compatibility
        assert_eq!(settings.brand_id, BrandId::Xiaomi);
        assert!(settings.supports_5ghz);
    }
}

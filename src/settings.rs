use anyhow::Result;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use crate::pattern::Pattern;

const NAMESPACE: &str = "lights";

/// Everything the user can change that should survive a power cycle.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub on: bool,
    pub brightness_percent: u8,
    pub pattern: Pattern,
    pub breathe_ms: u16,
    pub twinkle_interval_ms: u16,
}

/// Flash-backed storage for [`Settings`]. A missing or unreadable value falls back to the
/// matching field of the defaults passed to `load`, so a first boot (or a corrupted
/// entry) just starts from the defaults.
pub struct SettingsStore {
    nvs: EspNvs<NvsDefault>,
}

impl SettingsStore {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        Ok(Self {
            nvs: EspNvs::new(partition, NAMESPACE, true)?,
        })
    }

    pub fn load(&self, defaults: Settings) -> Settings {
        Settings {
            on: self
                .nvs
                .get_u8("on")
                .ok()
                .flatten()
                .map_or(defaults.on, |v| v != 0),
            brightness_percent: self
                .nvs
                .get_u8("brightness")
                .ok()
                .flatten()
                .map_or(defaults.brightness_percent, |v| v.min(100)),
            pattern: self
                .nvs
                .get_u8("pattern")
                .ok()
                .flatten()
                .map_or(defaults.pattern, Pattern::from_code),
            breathe_ms: self
                .nvs
                .get_u16("breathe")
                .ok()
                .flatten()
                .unwrap_or(defaults.breathe_ms),
            twinkle_interval_ms: self
                .nvs
                .get_u16("twinkle")
                .ok()
                .flatten()
                .unwrap_or(defaults.twinkle_interval_ms),
        }
    }

    pub fn save(&mut self, settings: Settings) -> Result<()> {
        self.nvs.set_u8("on", settings.on as u8)?;
        self.nvs.set_u8("brightness", settings.brightness_percent)?;
        self.nvs.set_u8("pattern", settings.pattern.to_code())?;
        self.nvs.set_u16("breathe", settings.breathe_ms)?;
        self.nvs.set_u16("twinkle", settings.twinkle_interval_ms)?;
        Ok(())
    }
}

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};

use crate::lights::{
    MAX_BREATHE_MS, MAX_TWINKLE_INTERVAL_MS, MIN_BREATHE_MS, MIN_TWINKLE_INTERVAL_MS,
};
use crate::pattern::Pattern;

/// Light state shared between the physical button, the HTTP handlers (each of which
/// runs on its own server thread), and the main loop that actually drives the LEDs.
pub struct SharedState {
    on: AtomicBool,
    brightness_percent: AtomicU8,
    pattern: AtomicU8,
    breathe_ms: AtomicU16,
    twinkle_interval_ms: AtomicU16,
    /// Latest solar sense-pin reading; written by the main loop, shown on the web page.
    solar_mv: AtomicU16,
}

impl SharedState {
    pub fn new(
        on: bool,
        brightness_percent: u8,
        pattern: Pattern,
        breathe_ms: u16,
        twinkle_interval_ms: u16,
    ) -> Self {
        Self {
            on: AtomicBool::new(on),
            brightness_percent: AtomicU8::new(brightness_percent.min(100)),
            pattern: AtomicU8::new(pattern.to_code()),
            breathe_ms: AtomicU16::new(
                breathe_ms.clamp(MIN_BREATHE_MS as u16, MAX_BREATHE_MS as u16),
            ),
            twinkle_interval_ms: AtomicU16::new(twinkle_interval_ms.clamp(
                MIN_TWINKLE_INTERVAL_MS as u16,
                MAX_TWINKLE_INTERVAL_MS as u16,
            )),
            solar_mv: AtomicU16::new(0),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }

    pub fn brightness_percent(&self) -> u8 {
        self.brightness_percent.load(Ordering::Relaxed)
    }

    pub fn set_brightness_percent(&self, percent: u8) {
        self.brightness_percent
            .store(percent.min(100), Ordering::Relaxed);
    }

    pub fn pattern(&self) -> Pattern {
        Pattern::from_code(self.pattern.load(Ordering::Relaxed))
    }

    pub fn set_pattern(&self, pattern: Pattern) {
        self.pattern.store(pattern.to_code(), Ordering::Relaxed);
    }

    pub fn breathe_ms(&self) -> u16 {
        self.breathe_ms.load(Ordering::Relaxed)
    }

    pub fn set_breathe_ms(&self, ms: u16) {
        let ms = ms.clamp(MIN_BREATHE_MS as u16, MAX_BREATHE_MS as u16);
        self.breathe_ms.store(ms, Ordering::Relaxed);
    }

    pub fn twinkle_interval_ms(&self) -> u16 {
        self.twinkle_interval_ms.load(Ordering::Relaxed)
    }

    pub fn set_twinkle_interval_ms(&self, ms: u16) {
        let ms = ms.clamp(
            MIN_TWINKLE_INTERVAL_MS as u16,
            MAX_TWINKLE_INTERVAL_MS as u16,
        );
        self.twinkle_interval_ms.store(ms, Ordering::Relaxed);
    }

    pub fn solar_mv(&self) -> u16 {
        self.solar_mv.load(Ordering::Relaxed)
    }

    pub fn set_solar_mv(&self, mv: u16) {
        self.solar_mv.store(mv, Ordering::Relaxed);
    }
}

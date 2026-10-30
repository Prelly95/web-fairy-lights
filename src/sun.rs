use anyhow::Result;
use esp_idf_hal::adc::oneshot::config::{AdcChannelConfig, Calibration};
use esp_idf_hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_hal::adc::{attenuation, AdcChannel, Resolution};
use esp_idf_hal::gpio::ADCPin;

/// Wire the solar panel across a voltage divider into the sense pin so that full sun
/// reads close to VCC (3.3V) and darkness reads close to 0V - never feed the panel's
/// raw output straight into the pin unless you're certain its open-circuit voltage
/// stays under 3.3V, or you'll risk damaging the chip. Tune these two thresholds to
/// your actual panel/divider once it's wired up and you can see real readings in the
/// log. The gap between them (hysteresis) stops a reading hovering right at the edge -
/// e.g. a passing cloud at dusk - from flapping the lights on and off repeatedly.
const SUNNY_MV: u16 = 800;
const DARK_MV: u16 = 500;

/// Reads a solar panel wired to an ADC-capable pin and reports whether it currently
/// sees daylight, debounced with hysteresis between `DARK_MV` and `SUNNY_MV`.
pub struct SunSensor<'d, C: AdcChannel> {
    channel: AdcChannelDriver<'d, C, AdcDriver<'d, C::AdcUnit>>,
    is_sunny: bool,
    millivolts: u16,
}

impl<'d, C: AdcChannel> SunSensor<'d, C> {
    pub fn new(
        adc: AdcDriver<'d, C::AdcUnit>,
        pin: impl ADCPin<AdcChannel = C> + 'd,
    ) -> Result<Self> {
        let config = AdcChannelConfig {
            attenuation: attenuation::DB_12,
            resolution: Resolution::Resolution12Bit,
            calibration: Calibration::None,
        };
        let channel = AdcChannelDriver::new(adc, pin, &config)?;

        Ok(Self {
            channel,
            is_sunny: false,
            millivolts: 0,
        })
    }

    /// Re-reads the sensor and updates the debounced sun/no-sun state. A light sensor
    /// doesn't need tick-rate polling; call this every second or so.
    pub fn update(&mut self) -> Result<()> {
        let mv = self.channel.read()?;
        self.millivolts = mv;
        if mv >= SUNNY_MV {
            self.is_sunny = true;
        } else if mv <= DARK_MV {
            self.is_sunny = false;
        }
        // Between the two thresholds: hold whatever the last state was.

        log::info!(
            "sun sensor: {mv}mV (sunny >= {SUNNY_MV}mV, dark <= {DARK_MV}mV) -> sunny={}",
            self.is_sunny
        );

        Ok(())
    }

    pub fn is_sunny(&self) -> bool {
        self.is_sunny
    }

    /// The sense pin's voltage as of the last `update`, for display.
    pub fn millivolts(&self) -> u16 {
        self.millivolts
    }
}

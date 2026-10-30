mod button;
mod dns;
mod lights;
mod mdns;
mod pattern;
mod settings;
mod state;
mod sun;
mod web;
mod wifi;

use std::sync::Arc;

use esp_idf_hal::adc::oneshot::AdcDriver;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::{PinDriver, Pull};
use esp_idf_hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::units::FromValueType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::EspWifi;

use button::Button;
use lights::Lights;
use pattern::Pattern;
use settings::{Settings, SettingsStore};
use state::SharedState;
use sun::SunSensor;

/// How often the button is polled and the light animation is advanced.
const POLL_MS: u32 = 20;
/// How often the solar sense pin is read - it's an ambient light level, no need to
/// poll it anywhere near as often as the button/animation.
const SUN_POLL_MS: u32 = 1_000;
/// Ignore further button edges for this long after a press is registered.
const DEBOUNCE_MS: u32 = 200;
/// How long the access point stays up with nobody connected - counted from boot, and
/// again from the moment the last client leaves - before Wi-Fi is switched off to save
/// power. A power cycle brings it back.
const WIFI_IDLE_TIMEOUT_MS: u32 = 60_000;
/// A changed setting is written to flash only once it has stopped changing for this long,
/// so dragging a slider doesn't wear the flash with a write per step.
const SETTINGS_SAVE_DELAY_MS: u32 = 3_000;
/// Acknowledgement animation when a client connects.
const CONNECT_BLINK_FLASHES: u32 = 3;
const CONNECT_BLINK_MS: u32 = 120;
/// Initial brightness, as a percentage of the available duty range.
const DEFAULT_BRIGHTNESS_PERCENT: u32 = 50;
/// Initial animation pattern.
const DEFAULT_PATTERN: Pattern = Pattern::AlternateBreathe;
/// PWM frequency for both LEDC channels; `Lights` needs this too, to size fade steps.
/// Kept above ~20kHz (the top of human hearing) so the switching itself isn't audible
/// as a whine through the wiring/LEDs - 5kHz was well within earshot.
const PWM_FREQUENCY_HZ: u32 = 25_000;
/// Initial duration of one breathing leg (valid range 500-10,000ms).
const DEFAULT_BREATHE_MS: u32 = 2_000;
/// Initial twinkle jump interval (valid range 10-400ms).
const DEFAULT_TWINKLE_INTERVAL_MS: u32 = 200;

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise, some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(PWM_FREQUENCY_HZ.Hz()),
    )?;
    let side_a = LedcDriver::new(peripherals.ledc.channel0, &timer, peripherals.pins.gpio3)?;
    let side_b = LedcDriver::new(peripherals.ledc.channel1, &timer, peripherals.pins.gpio4)?;

    // Pick up wherever the lights were left before the last power-down; on a first boot
    // (or if the stored values can't be read) this is just the defaults.
    let mut settings_store = SettingsStore::open(nvs.clone())?;
    let startup = settings_store.load(Settings {
        on: true,
        brightness_percent: DEFAULT_BRIGHTNESS_PERCENT as u8,
        pattern: DEFAULT_PATTERN,
        breathe_ms: DEFAULT_BREATHE_MS as u16,
        twinkle_interval_ms: DEFAULT_TWINKLE_INTERVAL_MS as u16,
    });

    let mut lights = Lights::new(
        side_a,
        side_b,
        startup.brightness_percent as u32,
        startup.pattern,
        PWM_FREQUENCY_HZ,
        startup.breathe_ms as u32,
        startup.twinkle_interval_ms as u32,
    )?;

    let mut button = Button::new(
        PinDriver::input(peripherals.pins.gpio9, Pull::Up)?,
        POLL_MS,
        DEBOUNCE_MS,
    );

    let mut sun = SunSensor::new(AdcDriver::new(peripherals.adc1)?, peripherals.pins.gpio0)?;

    let shared_state = Arc::new(SharedState::new(
        startup.on,
        startup.brightness_percent,
        startup.pattern,
        startup.breathe_ms,
        startup.twinkle_interval_ms,
    ));

    // Only fails on a genuine setup failure; the access point itself has no
    // association step to retry.
    let ap_status = Arc::new(wifi::ApStatus::default());
    let wifi_result =
        wifi::start_access_point(peripherals.modem, sys_loop, nvs, ap_status.clone());
    let mut wifi: Option<EspWifi<'static>> = None;

    match wifi_result {
        Ok(started) => {
            // Same "keep it running, don't propagate a failure to start" approach as the
            // HTTP server below: without mDNS you just fall back to the IP address logged
            // above, no need to take the whole thing down over it.
            match mdns::start() {
                Ok(mdns) => core::mem::forget(mdns),
                Err(err) => {
                    log::warn!("mDNS failed to start ({err}); use the IP address above instead")
                }
            }

            // Same non-fatal handling: the page still works by IP without it.
            let ap_ip = started.ap_netif().get_ip_info().map(|info| info.ip);
            match ap_ip {
                Ok(ip) => {
                    if let Err(err) = dns::start(ip) {
                        log::warn!("DNS catch-all failed to start ({err}); phones may flag the network as having no internet")
                    }
                }
                Err(err) => log::warn!("couldn't read AP address for DNS ({err})"),
            }

            match web::start_server(shared_state.clone()) {
                Ok(server) => {
                    // Keep the HTTP server running for the lifetime of the program; it
                    // would otherwise be torn down as soon as it's dropped, and the
                    // variable is never read again after this point.
                    core::mem::forget(server);
                }
                Err(err) => {
                    log::warn!(
                        "HTTP server failed to start ({err}); continuing with button control only"
                    )
                }
            }

            // Held (not forgotten) so the main loop can switch it off when idle.
            wifi = Some(started);
        }
        Err(err) => log::warn!("Wi-Fi unavailable ({err}); continuing with button control only"),
    }

    log::info!("Starting fairy lights");

    // `Lights::new` starts with the lights on; the loop below switches them off on its
    // first pass if that's how they were left.
    let mut applied_on = true;
    let mut applied_brightness = shared_state.brightness_percent();
    let mut applied_pattern = shared_state.pattern();
    let mut applied_breathe_ms = shared_state.breathe_ms();
    let mut applied_twinkle_interval_ms = shared_state.twinkle_interval_ms();
    let mut sun_elapsed_ms = 0u32;
    let mut applied_sunny = sun.is_sunny();
    let mut wifi_idle_ms = 0u32;
    let mut wifi_running = wifi.is_some();
    let mut saved_settings = startup;
    let mut pending_settings = startup;
    let mut settings_quiet_ms = 0u32;

    loop {
        if button.poll_pressed() {
            shared_state.set_on(!shared_state.is_on());
        }

        sun_elapsed_ms += POLL_MS;
        if sun_elapsed_ms >= SUN_POLL_MS {
            sun_elapsed_ms = 0;
            sun.update()?;
            shared_state.set_solar_mv(sun.millivolts());

            let sunny = sun.is_sunny();
            if sunny != applied_sunny {
                applied_sunny = sunny;
            }
        }

        if let Some(driver) = wifi.as_mut().filter(|_| wifi_running) {
            if ap_status.client_count() > 0 {
                wifi_idle_ms = 0;
            } else {
                wifi_idle_ms += POLL_MS;
                if wifi_idle_ms >= WIFI_IDLE_TIMEOUT_MS {
                    log::info!("no Wi-Fi clients for a minute - switching Wi-Fi off to save power");
                    // Stopped rather than dropped: the HTTP server, DNS and mDNS are
                    // still running on top of the network interface, which dropping the
                    // driver would pull out from under them.
                    if let Err(err) = driver.stop() {
                        log::warn!("failed to stop Wi-Fi: {err}");
                    }
                    wifi_running = false;
                }
            }

            if ap_status.take_new_connection() {
                lights.blink(CONNECT_BLINK_FLASHES, CONNECT_BLINK_MS)?;
            }
        }

        // The sun sensor overrides the user's on/off setting rather than fighting it:
        // it forces the lights off while it's bright out, but doesn't touch what the
        // button/web UI actually asked for, so whatever was set resumes once it's dark.
        let on = shared_state.is_on() && !applied_sunny;
        if on != applied_on {
            applied_on = on;
            lights.set_on(on)?;
        }

        let brightness = shared_state.brightness_percent();
        if brightness != applied_brightness {
            applied_brightness = brightness;
            lights.set_brightness_percent(brightness as u32)?;
        }

        let pattern = shared_state.pattern();
        if pattern != applied_pattern {
            applied_pattern = pattern;
            lights.set_pattern(pattern)?;
        }

        let breathe_ms = shared_state.breathe_ms();
        if breathe_ms != applied_breathe_ms {
            applied_breathe_ms = breathe_ms;
            lights.set_breathe_ms(breathe_ms as u32)?;
        }

        let twinkle_interval_ms = shared_state.twinkle_interval_ms();
        if twinkle_interval_ms != applied_twinkle_interval_ms {
            applied_twinkle_interval_ms = twinkle_interval_ms;
            lights.set_twinkle_interval_ms(twinkle_interval_ms as u32)?;
        }

        // Persist what the user last chose, once it has settled.
        let current = Settings {
            on: shared_state.is_on(),
            brightness_percent: shared_state.brightness_percent(),
            pattern: shared_state.pattern(),
            breathe_ms: shared_state.breathe_ms(),
            twinkle_interval_ms: shared_state.twinkle_interval_ms(),
        };
        if current != pending_settings {
            pending_settings = current;
            settings_quiet_ms = 0;
        } else if pending_settings != saved_settings {
            settings_quiet_ms += POLL_MS;
            if settings_quiet_ms >= SETTINGS_SAVE_DELAY_MS {
                match settings_store.save(pending_settings) {
                    Ok(()) => saved_settings = pending_settings,
                    Err(err) => {
                        log::warn!("couldn't save settings ({err}); will retry on the next change");
                        saved_settings = pending_settings;
                    }
                }
            }
        }

        lights.tick(POLL_MS)?;

        FreeRtos::delay_ms(POLL_MS);
    }
}

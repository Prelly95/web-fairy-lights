use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::ledc::LedcDriver;

use crate::pattern::Pattern;

/// Valid range for how long one leg of a breathing cycle takes.
pub const MIN_BREATHE_MS: u32 = 500;
pub const MAX_BREATHE_MS: u32 = 10_000;
/// Never fade all the way to a literal 0 duty while animating - only the master on/off
/// toggle does that. Keeps a faint glow during breathing/twinkling instead of a harder,
/// more noticeable blink to fully dark.
const FLOOR_DUTY: u32 = 1;
/// A blink is at least this bright (percent of the duty range) so it stays visible even
/// when the lights are set very dim.
const BLINK_MIN_PERCENT: u32 = 50;
/// Valid range for how often each side jumps to a fresh random brightness while
/// twinkling.
pub const MIN_TWINKLE_INTERVAL_MS: u32 = 10;
pub const MAX_TWINKLE_INTERVAL_MS: u32 = 400;

/// The LEDC hardware's step-fade fields (step size, cycles held per step, and step
/// count) are each only 10 bits wide, so every one of them caps out at 1023.
const MAX_FADE_STEPS: u32 = 1023;
const MAX_STEP_CYCLES: u32 = 1023;

/// Drives two anti-parallel LED strings (side A on one GPIO, side B on the other)
/// through one of several animated patterns.
///
/// Both LEDC channels share one hardware timer, so their counters are in lockstep.
/// Side A's pulse starts at the top of the period (hpoint 0); side B's pulse is pushed
/// out by exactly half a period. As long as each channel's duty stays <= half_period,
/// their high times can never overlap, so the two channels are free to run at different
/// (or equal) duties without ever driving both GPIOs high at once - which is what makes
/// both the alternating and the synchronized patterns possible on the same two wires.
/// Hardware fades only ramp the duty register, not hpoint, so the offset survives
/// fading untouched.
pub struct Lights<'d> {
    side_a: LedcDriver<'d>,
    side_b: LedcDriver<'d>,
    half_period: u32,
    timer_frequency_hz: u32,
    on: bool,
    brightness_percent: u32,
    pattern: Pattern,
    // The duty we last commanded each side towards, used to size the next fade's steps.
    duty_a: u32,
    duty_b: u32,
    // Alternate/sync breathing: which half of the cycle we're in, how far into it, and
    // how long one leg takes (adjustable via `set_breathe_ms`).
    breathe_phase_high: bool,
    breathe_elapsed_ms: u32,
    breathe_ms: u32,
    // Twinkle: time left until each side jumps to a fresh random duty, and how often
    // that happens (adjustable via `set_twinkle_interval_ms`).
    twinkle_a_remaining_ms: u32,
    twinkle_b_remaining_ms: u32,
    twinkle_interval_ms: u32,
}

impl<'d> Lights<'d> {
    pub fn new(
        side_a: LedcDriver<'d>,
        mut side_b: LedcDriver<'d>,
        brightness_percent: u32,
        pattern: Pattern,
        timer_frequency_hz: u32,
        breathe_ms: u32,
        twinkle_interval_ms: u32,
    ) -> Result<Self> {
        let half_period = side_a.get_max_duty() / 2;
        side_b.set_hpoint(half_period)?;

        let mut lights = Self {
            side_a,
            side_b,
            half_period,
            timer_frequency_hz,
            on: true,
            brightness_percent: brightness_percent.min(100),
            pattern,
            duty_a: 0,
            duty_b: 0,
            breathe_phase_high: true,
            breathe_elapsed_ms: 0,
            breathe_ms: breathe_ms.clamp(MIN_BREATHE_MS, MAX_BREATHE_MS),
            twinkle_a_remaining_ms: 0,
            twinkle_b_remaining_ms: 0,
            twinkle_interval_ms: twinkle_interval_ms
                .clamp(MIN_TWINKLE_INTERVAL_MS, MAX_TWINKLE_INTERVAL_MS),
        };
        lights.activate_current_pattern()?;
        Ok(lights)
    }

    fn on_duty(&self) -> u32 {
        self.half_period * self.brightness_percent / 100
    }

    /// Fades side A towards `target` over (approximately) `desired_ms`, no matter how
    /// small the duty change is.
    ///
    /// `fade_with_time` asks the hardware to pick the step size and per-step hold time
    /// itself, and it does that by spreading the *whole* requested duration over the
    /// duty delta - so a small brightness change held for a long time forces very few,
    /// very long steps. Those step holds are a 10-bit hardware field, so past a point
    /// the driver can't represent them, logs "LEDC FADE TOO SLOW", and silently shortens
    /// the fade instead - which is why the actual fade time crept around with brightness.
    /// Driving `fade_with_step` ourselves fixes the per-step hold time (`cycles_per_step`)
    /// derived from `desired_ms` and grows/shrinks the step *size* to fit the actual duty
    /// delta into that same duration instead, which stays within the hardware's limits
    /// for any delta the two channels can realistically have here.
    fn fade_side_a(&mut self, target: u32, desired_ms: u32) -> Result<()> {
        fade_channel(
            &mut self.side_a,
            self.duty_a,
            target,
            desired_ms,
            self.timer_frequency_hz,
        )?;
        self.duty_a = target;
        Ok(())
    }

    fn fade_side_b(&mut self, target: u32, desired_ms: u32) -> Result<()> {
        fade_channel(
            &mut self.side_b,
            self.duty_b,
            target,
            desired_ms,
            self.timer_frequency_hz,
        )?;
        self.duty_b = target;
        Ok(())
    }

    /// Flashes both sides together `flashes` times, `flash_ms` lit and `flash_ms` dark,
    /// then hands control back to whatever the lights were doing (or leaves them off, if
    /// they were off). Blocks for the whole animation - it's a short acknowledgement, not
    /// something to run during normal operation.
    pub fn blink(&mut self, flashes: u32, flash_ms: u32) -> Result<()> {
        self.side_a.fade_stop()?;
        self.side_b.fade_stop()?;
        let lit = self.half_period * self.brightness_percent.max(BLINK_MIN_PERCENT) / 100;

        for _ in 0..flashes {
            self.side_a.set_duty(lit)?;
            self.side_b.set_duty(lit)?;
            FreeRtos::delay_ms(flash_ms);
            self.side_a.set_duty(0)?;
            self.side_b.set_duty(0)?;
            FreeRtos::delay_ms(flash_ms);
        }

        self.duty_a = 0;
        self.duty_b = 0;
        if self.on {
            self.activate_current_pattern()?;
        }
        Ok(())
    }

    /// (Re)starts whatever the current pattern needs from a clean slate: called when the
    /// lights are switched on, when the pattern changes, and when the brightness changes
    /// (so the new duty takes effect immediately instead of waiting for the next event).
    fn activate_current_pattern(&mut self) -> Result<()> {
        match self.pattern {
            Pattern::AlternateBreathe | Pattern::SyncBreathe => {
                self.breathe_phase_high = true;
                self.start_breathe_leg()?;
            }
            Pattern::Twinkle => {
                // Twinkle jumps duty directly rather than fading; stop any fade left
                // over from a previous pattern so it can't keep nudging the duty.
                self.side_a.fade_stop()?;
                self.side_b.fade_stop()?;
                // Force both sides to jump on the very next tick.
                self.twinkle_a_remaining_ms = 0;
                self.twinkle_b_remaining_ms = 0;
            }
            Pattern::Solid => {
                self.side_a.fade_stop()?;
                self.side_b.fade_stop()?;
                let on_duty = self.on_duty();
                self.side_a.set_duty(on_duty)?;
                self.duty_a = on_duty;
                self.side_b.set_duty(on_duty)?;
                self.duty_b = on_duty;
            }
        }
        Ok(())
    }

    /// The (side A, side B) duty targets for the current breathing phase, or `None` if
    /// the active pattern isn't a breathing one.
    fn breathe_targets(&self) -> Option<(u32, u32)> {
        let on_duty = self.on_duty();
        match (self.pattern, self.breathe_phase_high) {
            (Pattern::AlternateBreathe, true) => Some((on_duty, FLOOR_DUTY)),
            (Pattern::AlternateBreathe, false) => Some((FLOOR_DUTY, on_duty)),
            (Pattern::SyncBreathe, true) => Some((on_duty, on_duty)),
            (Pattern::SyncBreathe, false) => Some((FLOOR_DUTY, FLOOR_DUTY)),
            _ => None,
        }
    }

    fn start_breathe_leg(&mut self) -> Result<()> {
        let Some((target_a, target_b)) = self.breathe_targets() else {
            return Ok(());
        };
        self.fade_side_a(target_a, self.breathe_ms)?;
        self.fade_side_b(target_b, self.breathe_ms)?;
        self.breathe_elapsed_ms = 0;
        Ok(())
    }

    /// Jumps straight to the current breathing phase's targets (no fade) and resets the
    /// leg's clock, so a brightness change takes effect immediately instead of being
    /// smoothly (and slowly) faded into over the rest of the in-flight leg.
    fn snap_breathe_targets(&mut self) -> Result<()> {
        let Some((target_a, target_b)) = self.breathe_targets() else {
            return Ok(());
        };
        self.side_a.fade_stop()?;
        self.side_b.fade_stop()?;
        self.side_a.set_duty(target_a)?;
        self.duty_a = target_a;
        self.side_b.set_duty(target_b)?;
        self.duty_b = target_b;
        self.breathe_elapsed_ms = 0;
        Ok(())
    }

    fn retarget_twinkle_a(&mut self) -> Result<()> {
        let target = random_range(FLOOR_DUTY, self.on_duty().max(FLOOR_DUTY));
        self.side_a.set_duty(target)?;
        self.duty_a = target;
        self.twinkle_a_remaining_ms = self.twinkle_interval_ms;
        Ok(())
    }

    fn retarget_twinkle_b(&mut self) -> Result<()> {
        let target = random_range(FLOOR_DUTY, self.on_duty().max(FLOOR_DUTY));
        self.side_b.set_duty(target)?;
        self.duty_b = target;
        self.twinkle_b_remaining_ms = self.twinkle_interval_ms;
        Ok(())
    }

    pub fn set_on(&mut self, on: bool) -> Result<()> {
        if on == self.on {
            return Ok(());
        }
        self.on = on;

        if on {
            self.activate_current_pattern()?;
        } else {
            self.side_a.fade_stop()?;
            self.side_b.fade_stop()?;
            self.side_a.set_duty(0)?;
            self.duty_a = 0;
            self.side_b.set_duty(0)?;
            self.duty_b = 0;
        }
        Ok(())
    }

    pub fn set_brightness_percent(&mut self, percent: u32) -> Result<()> {
        let percent = percent.min(100);
        if percent == self.brightness_percent {
            return Ok(());
        }
        self.brightness_percent = percent;

        if !self.on {
            return Ok(());
        }

        match self.pattern {
            // Breathing already has its own animation running; only the brightness
            // ceiling changed, so snap to it instantly rather than smoothly (and
            // slowly, over the rest of the in-flight leg) fading into it.
            Pattern::AlternateBreathe | Pattern::SyncBreathe => self.snap_breathe_targets()?,
            Pattern::Twinkle | Pattern::Solid => self.activate_current_pattern()?,
        }
        Ok(())
    }

    pub fn set_breathe_ms(&mut self, ms: u32) -> Result<()> {
        let ms = ms.clamp(MIN_BREATHE_MS, MAX_BREATHE_MS);
        if ms == self.breathe_ms {
            return Ok(());
        }
        self.breathe_ms = ms;

        // Restart the breathing cycle from the top on the new schedule, rather than
        // leaving the in-flight leg to finish out the old (possibly much longer) one.
        if self.on
            && matches!(
                self.pattern,
                Pattern::AlternateBreathe | Pattern::SyncBreathe
            )
        {
            self.breathe_phase_high = true;
            self.start_breathe_leg()?;
        }
        Ok(())
    }

    pub fn set_twinkle_interval_ms(&mut self, ms: u32) -> Result<()> {
        let ms = ms.clamp(MIN_TWINKLE_INTERVAL_MS, MAX_TWINKLE_INTERVAL_MS);
        if ms == self.twinkle_interval_ms {
            return Ok(());
        }
        self.twinkle_interval_ms = ms;

        // Jump both sides immediately on the new cadence instead of waiting out
        // whatever was left on the old one.
        if self.on && self.pattern == Pattern::Twinkle {
            self.twinkle_a_remaining_ms = 0;
            self.twinkle_b_remaining_ms = 0;
        }
        Ok(())
    }

    pub fn set_pattern(&mut self, pattern: Pattern) -> Result<()> {
        if pattern == self.pattern {
            return Ok(());
        }
        self.pattern = pattern;

        if self.on {
            self.activate_current_pattern()?;
        }
        Ok(())
    }

    /// Call once per polling tick to advance whichever pattern is active.
    pub fn tick(&mut self, elapsed_ms: u32) -> Result<()> {
        if !self.on {
            return Ok(());
        }

        match self.pattern {
            Pattern::AlternateBreathe | Pattern::SyncBreathe => {
                self.breathe_elapsed_ms += elapsed_ms;
                if self.breathe_elapsed_ms >= self.breathe_ms {
                    self.breathe_phase_high = !self.breathe_phase_high;
                    self.start_breathe_leg()?;
                }
            }
            Pattern::Twinkle => {
                self.twinkle_a_remaining_ms =
                    self.twinkle_a_remaining_ms.saturating_sub(elapsed_ms);
                if self.twinkle_a_remaining_ms == 0 {
                    self.retarget_twinkle_a()?;
                }

                self.twinkle_b_remaining_ms =
                    self.twinkle_b_remaining_ms.saturating_sub(elapsed_ms);
                if self.twinkle_b_remaining_ms == 0 {
                    self.retarget_twinkle_b()?;
                }
            }
            Pattern::Solid => {}
        }
        Ok(())
    }
}

/// Fades one LEDC channel from `from` to `to` over (approximately) `desired_ms`, by
/// deriving a step size that fits the actual duty delta into a fixed per-step hold time
/// instead of asking the hardware to size steps to fit the whole duration (see the
/// doc comment on `Lights::fade_side_a`). Falls back to setting the duty immediately if
/// there's nothing to fade.
fn fade_channel(
    channel: &mut LedcDriver<'_>,
    from: u32,
    to: u32,
    desired_ms: u32,
    timer_frequency_hz: u32,
) -> Result<()> {
    let delta = to.abs_diff(from);
    if delta == 0 {
        channel.set_duty(to)?;
        return Ok(());
    }

    let total_cycles = (desired_ms as u64 * timer_frequency_hz as u64 / 1000) as u32;
    let steps = delta.min(MAX_FADE_STEPS);
    let step_size = delta.div_ceil(steps);
    let cycles_per_step = (total_cycles / steps).clamp(1, MAX_STEP_CYCLES);

    channel.fade_with_step(to, step_size, cycles_per_step, false)?;
    Ok(())
}

/// Inclusive random value in `[min, max]`, backed by the hardware RNG. Falls back to
/// `min` if the range is empty.
fn random_range(min: u32, max: u32) -> u32 {
    if max <= min {
        return min;
    }
    let span = max - min + 1;
    min + unsafe { esp_idf_svc::sys::esp_random() } % span
}

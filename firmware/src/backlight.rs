//! AMOLED backlight (GPIO47) with PWM dimming and idle auto-dim/off.
//!
//! State machine driven from the main loop's `loop_tick` (3 ms per tick):
//!
//! ```text
//!     activity                activity
//!  ┌──────────┐            ┌──────────┐
//!  ▼          │            ▼          │
//! Active ──── dim_after ──▶ Dim ───── off_after ──▶ Off
//!                                                    │
//!                                              wake (encoder
//!                                              or first touch)
//!                                                    │
//!                                                    ▼
//!                                                 Active
//! ```
//!
//! `wake_from_off` returns `true` to the caller so a touch that woke the
//! screen can be discarded instead of toggling mute under the user's finger.
//!
//! Runtime-configurable: the companion can push brightness + timeouts via
//! `SetBacklight`.

extern crate alloc;
use alloc::boxed::Box;

use esp_hal::gpio::DriveMode;
use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::ledc::channel::{self, Channel, ChannelIFace};
use esp_hal::ledc::timer::{self, Timer, TimerIFace};
use esp_hal::ledc::{Ledc, LowSpeed, LSGlobalClkSource};
use esp_hal::peripherals::LEDC;
use esp_hal::time::Rate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Dim,
    Off,
}

/// Idle-driven backlight controller. Owns the LEDC channel for GPIO47.
///
/// The Ledc and Timer are leaked to `'static` because both `Channel` and the
/// LEDC peripheral live for the full firmware run, and self-referential
/// structs need that lifetime laundering. Leak is safe — no Drop impls
/// disable the peripheral, and the firmware never tears down.
pub struct Backlight {
    channel: Channel<'static, LowSpeed>,

    state: State,
    /// Loop tick when the most recent activity (encoder turn, touch, or
    /// host-driven volume/mute change) was observed. Used to schedule the
    /// Active → Dim → Off transitions.
    last_activity_tick: u32,

    /// Brightness (0–100) when Active. Dim is `active_pct / 4` (min 5).
    active_pct: u8,
    /// Ticks of idle before transitioning Active → Dim.
    dim_after_ticks: u32,
    /// Additional ticks of idle (after the dim transition) before Dim → Off.
    off_after_ticks: u32,
}

/// Default config — tuned for a desktop knob. 30 s to dim, then 90 s to fully
/// off (so 2 minutes total of idleness before the screen blanks).
pub const DEFAULT_ACTIVE_PCT: u8 = 100;
pub const DEFAULT_DIM_AFTER_SECS: u16 = 30;
pub const DEFAULT_OFF_AFTER_SECS: u16 = 90;

/// Main loop runs at ~3 ms/tick; convert seconds to ticks for comparisons.
const TICKS_PER_SEC: u32 = 333;

impl Backlight {
    pub fn new(ledc_peripheral: LEDC<'static>, pin: impl PeripheralOutput<'static> + 'static) -> Self {
        let mut ledc = Ledc::new(ledc_peripheral);
        ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
        let ledc: &'static Ledc<'static> = Box::leak(Box::new(ledc));

        let mut timer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
        timer
            .configure(timer::config::Config {
                duty: timer::config::Duty::Duty8Bit,
                clock_source: timer::LSClockSource::APBClk,
                frequency: Rate::from_khz(50),
            })
            .expect("backlight timer configure");
        let timer: &'static Timer<'static, LowSpeed> = Box::leak(Box::new(timer));

        let mut channel = ledc.channel(channel::Number::Channel0, pin);
        channel
            .configure(channel::config::Config {
                timer,
                duty_pct: DEFAULT_ACTIVE_PCT,
                drive_mode: DriveMode::PushPull,
            })
            .expect("backlight channel configure");

        Self {
            channel,
            state: State::Active,
            last_activity_tick: 0,
            active_pct: DEFAULT_ACTIVE_PCT,
            dim_after_ticks: secs_to_ticks(DEFAULT_DIM_AFTER_SECS),
            off_after_ticks: secs_to_ticks(DEFAULT_OFF_AFTER_SECS),
        }
    }

    /// Reset the idle timer and restore full brightness if dimmed/off.
    /// Call on any user-driven activity that should keep the screen alive.
    pub fn notify_activity(&mut self, now_tick: u32) {
        self.last_activity_tick = now_tick;
        if self.state != State::Active {
            self.set_state(State::Active);
        }
    }

    /// If currently Off, transition to Active and return `true` so the caller
    /// can suppress the wake event (e.g. a tap that should not also toggle
    /// mute). Returns `false` when in Active or Dim — those gestures still
    /// fire normally because the screen was readable.
    pub fn wake_from_off(&mut self, now_tick: u32) -> bool {
        let was_off = self.state == State::Off;
        self.notify_activity(now_tick);
        was_off
    }

    /// Run the idle state machine. Cheap — call every main-loop iteration.
    pub fn tick(&mut self, now_tick: u32) {
        let idle = now_tick.wrapping_sub(self.last_activity_tick);
        let next = match self.state {
            State::Active if idle >= self.dim_after_ticks => State::Dim,
            State::Dim
                if idle.saturating_sub(self.dim_after_ticks) >= self.off_after_ticks =>
            {
                State::Off
            }
            other => other,
        };
        if next != self.state {
            self.set_state(next);
        }
    }

    /// Update the Active brightness (1–100). Takes effect immediately if
    /// currently Active; otherwise applied on the next wake.
    pub fn set_active_pct(&mut self, pct: u8) {
        self.active_pct = pct.clamp(1, 100);
        if self.state == State::Active {
            let _ = self.channel.set_duty(self.active_pct);
        }
    }

    /// Update the dim/off timeouts. `0` disables that transition.
    pub fn set_timeouts(&mut self, dim_after_secs: u16, off_after_secs: u16) {
        self.dim_after_ticks = if dim_after_secs == 0 {
            u32::MAX
        } else {
            secs_to_ticks(dim_after_secs)
        };
        self.off_after_ticks = if off_after_secs == 0 {
            u32::MAX
        } else {
            secs_to_ticks(off_after_secs)
        };
    }

    fn set_state(&mut self, state: State) {
        self.state = state;
        let pct = match state {
            State::Active => self.active_pct,
            State::Dim => (self.active_pct / 4).max(5),
            State::Off => 0,
        };
        let _ = self.channel.set_duty(pct);
    }
}

fn secs_to_ticks(secs: u16) -> u32 {
    (secs as u32).saturating_mul(TICKS_PER_SEC)
}

use core::cell::RefCell;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use critical_section::Mutex;
use esp_hal::gpio::{Event, Input, InputConfig, InputPin, Pull};
use esp_hal::interrupt::Priority;
use esp_hal::time::Instant;

/// Net step count updated by the GPIO ISR.
/// Positive = CW (pin_a rising edge), negative = CCW (pin_b rising edge).
static COUNT: AtomicI32 = AtomicI32::new(0);

/// Timestamp (µs since boot, truncated to u32) of the last accepted count
/// from EITHER channel. Both channels check against this, giving two guarantees:
///
/// 1. Same-channel bounce: rapid re-fires on the same pin are ignored.
/// 2. Cross-channel guard: after a CW step, a delayed spurious CCW pulse
///    (pin_b briefly firing during CW rotation) is suppressed, and vice-versa.
///
/// AtomicU32 wraps every ~71 min; wrapping_sub handles rollover correctly.
static LAST_COUNT_US: AtomicU32 = AtomicU32::new(0);

/// Minimum µs between any two accepted counts (same OR opposite channel).
///
/// 8 ms suppresses:
///   - Contact bounce (typically < 2 ms).
///   - Delayed cross-channel pulses from mechanical coupling.
///
/// Maximum reliable step rate at 8 ms = 125 steps/sec ≈ 6 rev/sec on a
/// 20-detent encoder — well above any realistic volume-knob speed.
const GUARD_US: u32 = 8_000;

/// Pins stored for ISR access. Populated once in `Encoder::new` before the
/// interrupt is enabled, then only touched inside the ISR.
static PINS: Mutex<RefCell<Option<(Input<'static>, Input<'static>)>>> =
    Mutex::new(RefCell::new(None));

/// Interrupt-driven rotary encoder with hardware-timer debounce.
///
/// Rising edges on the encoder channels are counted directly in the GPIO ISR,
/// so no steps are missed regardless of main-loop timing or display flush
/// latency. A unified cross-channel guard suppresses both contact bounce and
/// the spurious opposite-direction pulses that a polling driver would miss.
pub struct Encoder;

impl Encoder {
    /// Initialise the encoder and register the GPIO interrupt handler.
    ///
    /// `io` must be the `Io` driver (wraps `IO_MUX`) — used once to register
    /// the interrupt handler, then no longer needed.
    pub fn new<'d>(
        gpio_a: impl InputPin + 'd,
        gpio_b: impl InputPin + 'd,
        io: &mut esp_hal::gpio::Io<'_>,
    ) -> Self {
        let config = InputConfig::default().with_pull(Pull::Up);

        // SAFETY: GPIO peripheral singletons live for the entire firmware
        // lifetime. The transmute to 'static is required to place them in the
        // ISR-accessible static. Correct for no_std firmware that never exits.
        let mut pin_a: Input<'static> =
            unsafe { core::mem::transmute(Input::new(gpio_a, config)) };
        let mut pin_b: Input<'static> =
            unsafe { core::mem::transmute(Input::new(gpio_b, config)) };

        pin_a.listen(Event::RisingEdge);
        pin_b.listen(Event::RisingEdge);

        critical_section::with(|cs| {
            PINS.borrow_ref_mut(cs).replace((pin_a, pin_b));
        });

        io.set_interrupt_handler(gpio_encoder_handler);

        Self
    }

    /// No-op — steps are accumulated by the GPIO ISR.
    pub fn poll(&mut self) {}

    /// Net accumulated step count. Positive = CW, negative = CCW.
    pub fn get(&self) -> i32 {
        COUNT.load(Ordering::Relaxed)
    }
}

/// GPIO interrupt handler — fires on every rising edge of either encoder pin.
///
/// Both channels share a single gate timestamp. Whichever channel fires first
/// wins; the other is suppressed for GUARD_US regardless of which channel it is.
/// The interrupt status is always cleared so the next real edge can fire again.
#[esp_hal::handler(priority = Priority::Priority2)]
fn gpio_encoder_handler() {
    let now_us = Instant::now().duration_since_epoch().as_micros() as u32;
    let last = LAST_COUNT_US.load(Ordering::Relaxed);
    let elapsed = now_us.wrapping_sub(last);

    critical_section::with(|cs| {
        if let Some((pin_a, pin_b)) = PINS.borrow_ref_mut(cs).as_mut() {
            if pin_a.is_interrupt_set() {
                if elapsed >= GUARD_US {
                    COUNT.fetch_add(1, Ordering::Relaxed);
                    LAST_COUNT_US.store(now_us, Ordering::Relaxed);
                }
                pin_a.clear_interrupt();
            }
            if pin_b.is_interrupt_set() {
                if elapsed >= GUARD_US {
                    COUNT.fetch_sub(1, Ordering::Relaxed);
                    LAST_COUNT_US.store(now_us, Ordering::Relaxed);
                }
                pin_b.clear_interrupt();
            }
        }
    });
}

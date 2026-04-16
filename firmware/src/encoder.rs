use esp_hal::gpio::{Input, InputConfig, InputPin, Pull};

/// Software-debounced rotary encoder, matching the Waveshare C reference driver.
///
/// Call [`Encoder::poll`] every ~3 ms. A directional step is registered only
/// after the active channel has been LOW for at least `DEBOUNCE_TICKS` polls
/// before its rising edge — filtering out mechanical contact bounce.
///
/// Rising edge on pin_a → +1 (CW), rising edge on pin_b → -1 (CCW).
pub struct Encoder<'d> {
    pin_a: Input<'d>,
    pin_b: Input<'d>,
    level_a: bool,
    level_b: bool,
    debounce_a: u8,
    debounce_b: u8,
    count: i32,
}

/// Minimum number of 3 ms ticks the pin must be held LOW before a rising
/// edge is counted. Mirrors DEBOUNCE_TICKS in the Waveshare C driver.
const DEBOUNCE_TICKS: u8 = 2;

impl<'d> Encoder<'d> {
    pub fn new(gpio_a: impl InputPin + 'd, gpio_b: impl InputPin + 'd) -> Self {
        let config = InputConfig::default().with_pull(Pull::Up);
        let pin_a = Input::new(gpio_a, config);
        let pin_b = Input::new(gpio_b, config);
        let level_a = pin_a.is_high();
        let level_b = pin_b.is_high();
        Self {
            pin_a,
            pin_b,
            level_a,
            level_b,
            debounce_a: 0,
            debounce_b: 0,
            count: 0,
        }
    }

    /// Sample both encoder pins. Call every ~3 ms.
    pub fn poll(&mut self) {
        let a = self.pin_a.is_high();
        let b = self.pin_b.is_high();

        // Debounce channel A (CW → increment)
        if !a {
            // Pin is LOW: reset debounce on the falling edge, accumulate while held low.
            if a != self.level_a {
                self.debounce_a = 0;
            } else {
                self.debounce_a = self.debounce_a.saturating_add(1);
            }
        } else if a != self.level_a {
            // Rising edge: only count if pin was LOW long enough.
            self.debounce_a = self.debounce_a.saturating_add(1);
            if self.debounce_a >= DEBOUNCE_TICKS {
                self.count += 1;
            }
            self.debounce_a = 0;
        } else {
            self.debounce_a = 0;
        }
        self.level_a = a;

        // Debounce channel B (CCW → decrement), same logic
        if !b {
            if b != self.level_b {
                self.debounce_b = 0;
            } else {
                self.debounce_b = self.debounce_b.saturating_add(1);
            }
        } else if b != self.level_b {
            self.debounce_b = self.debounce_b.saturating_add(1);
            if self.debounce_b >= DEBOUNCE_TICKS {
                self.count -= 1;
            }
            self.debounce_b = 0;
        } else {
            self.debounce_b = 0;
        }
        self.level_b = b;
    }

    /// Returns the accumulated step count.
    ///
    /// Positive = CW, negative = CCW.
    pub fn get(&self) -> i32 {
        self.count
    }
}

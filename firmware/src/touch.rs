use esp_hal::gpio::{Input, InputConfig, InputPin, Level, Output, OutputConfig, OutputPin, Pull};
use esp_hal::i2c::master::I2c;
use esp_hal::delay::Delay;
use esp_hal::Blocking;

const CST816_ADDR: u8 = 0x15;

// Registers
// Registers — we read 0x01..0x06 as a block, so only start/chip/sleep are used directly
const REG_GESTURE_ID: u8 = 0x01;
const REG_CHIP_ID: u8 = 0xA7;
const REG_DIS_AUTOSLEEP: u8 = 0xFE;

/// Gesture types reported by the CST816.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    None,
    SwipeUp,
    SwipeDown,
    SwipeLeft,
    SwipeRight,
    SingleTap,
    DoubleTap,
    LongPress,
}

impl Gesture {
    fn from_id(id: u8) -> Self {
        match id {
            0x01 => Gesture::SwipeUp,
            0x02 => Gesture::SwipeDown,
            0x03 => Gesture::SwipeLeft,
            0x04 => Gesture::SwipeRight,
            0x05 => Gesture::SingleTap,
            0x0B => Gesture::DoubleTap,
            0x0C => Gesture::LongPress,
            _ => Gesture::None,
        }
    }
}

/// Touch event: position + gesture.
#[derive(Debug, Clone, Copy)]
pub struct TouchEvent {
    pub x: u16,
    pub y: u16,
    pub gesture: Gesture,
}

/// CST816 capacitive touch driver over I2C.
///
/// Does not own the I2C bus — pass `&mut I2c` to each method so the bus
/// can be shared with other devices (DRV2605 haptic).
pub struct Cst816<'d> {
    irq: Input<'d>,
    _rst: Output<'d>,
    last_gesture: Gesture,
    /// Updated on every `read()` call from the `num_points` register.
    /// More reliable than the IRQ pin for detecting a sustained hold.
    finger_down: bool,
}

impl<'d> Cst816<'d> {
    /// Create and initialize the touch driver.
    ///
    /// Performs hardware reset and disables auto-sleep.
    pub fn new(
        i2c: &mut I2c<'_, Blocking>,
        irq_pin: impl InputPin + 'd,
        rst_pin: impl OutputPin + 'd,
    ) -> Self {
        let irq = Input::new(irq_pin, InputConfig::default().with_pull(Pull::Up));
        let mut rst = Output::new(rst_pin, Level::High, OutputConfig::default());

        // Hardware reset: LOW 30ms, HIGH 50ms (matches Waveshare C driver)
        let delay = Delay::new();
        rst.set_low();
        delay.delay_millis(30);
        rst.set_high();
        delay.delay_millis(50);

        // Read chip ID for verification
        let mut id = [0u8; 1];
        if i2c.write_read(CST816_ADDR, &[REG_CHIP_ID], &mut id).is_ok() {
            log::info!("CST816 chip ID: 0x{:02X}", id[0]);
        }

        // Disable auto-sleep so touch stays responsive
        let _ = i2c.write(CST816_ADDR, &[REG_DIS_AUTOSLEEP, 0x01]);

        // Note: DoubleTap (0x0B) does not work on CST816D (chip ID 0xB6) despite
        // setting MotionMask bit 2. All other gestures work fine.

        Self { irq, _rst: rst, last_gesture: Gesture::None, finger_down: false }
    }

    /// Returns true if the touch IRQ pin is asserted (active LOW).
    pub fn is_touched(&self) -> bool {
        self.irq.is_low()
    }

    /// Returns true if a finger is currently on the screen, based on the
    /// `num_points` register read during the last `read()` call.
    pub fn is_finger_down(&self) -> bool {
        self.finger_down
    }

    /// Poll for a touch event.
    ///
    /// Always reads the registers rather than gating on IRQ, because tap/long-press
    /// gestures are reported at finger-lift when IRQ has already gone HIGH.
    pub fn read(&mut self, i2c: &mut I2c<'_, Blocking>) -> Option<TouchEvent> {
        // Read registers 0x01..0x06 in one transaction (gesture, num_points, x, y)
        let mut buf = [0u8; 6];
        if i2c.write_read(CST816_ADDR, &[REG_GESTURE_ID], &mut buf).is_err() {
            return None;
        }

        let gesture = Gesture::from_id(buf[0]);
        let num_points = buf[1];

        // Cache finger presence from the register — more reliable than the IRQ
        // pin, which only pulses briefly and is high again by finger-lift time.
        self.finger_down = num_points > 0 && num_points != 0xFF;

        // No finger and no gesture — nothing happening
        if num_points == 0 && gesture == Gesture::None {
            self.last_gesture = Gesture::None;
            return None;
        }
        if num_points == 0xFF {
            return None;
        }

        // Only report a gesture once — skip if same as last time
        if gesture == self.last_gesture {
            return None;
        }
        self.last_gesture = gesture;

        // Skip None gestures (raw touch points)
        if gesture == Gesture::None {
            return None;
        }

        let x = ((buf[2] as u16 & 0x0F) << 8) | buf[3] as u16;
        let y = ((buf[4] as u16 & 0x0F) << 8) | buf[5] as u16;

        Some(TouchEvent { x, y, gesture })
    }
}

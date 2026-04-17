use esp_hal::i2c::master::I2c;
use esp_hal::Blocking;

const DRV2605_ADDR: u8 = 0x5A;

// Registers
const REG_STATUS: u8 = 0x00;
const REG_MODE: u8 = 0x01;
const REG_LIBRARY: u8 = 0x03;
const REG_WAVEFORM: u8 = 0x04; // Waveform sequencer slot 1
const REG_GO: u8 = 0x0C;
const REG_RATED_VOLTAGE: u8 = 0x16;
const REG_OD_CLAMP: u8 = 0x17;
const REG_FEEDBACK: u8 = 0x1A;
const REG_CONTROL3: u8 = 0x1D;

/// Built-in ROM effect IDs (ERM library).
/// Full list in DRV2605 datasheet Table 11.2.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Effect {
    StrongClick = 1,
    SharpClick = 4,
    SoftBump = 7,
    ShortBuzz = 47,
    MediumClick = 10,
    Tick = 27,
}

/// Stateless DRV2605 haptic driver.
///
/// Borrows the I2C bus only when firing an effect, so the bus can be
/// shared with other devices (CST816 touch).
pub struct Drv2605 {
    _private: (),
}

impl Drv2605 {
    /// Initialize the DRV2605 on the given I2C bus.
    ///
    /// Sets up internal-trigger mode with the ERM library.
    /// Returns the driver handle (stateless — the bus is returned).
    pub fn init(i2c: &mut I2c<'_, Blocking>) -> Self {
        // Read status to verify device is present
        let mut status = [0u8; 1];
        if i2c.write_read(DRV2605_ADDR, &[REG_STATUS], &mut status).is_ok() {
            log::info!("DRV2605 status: 0x{:02X}", status[0]);
        } else {
            log::warn!("DRV2605 not found at 0x{:02X}", DRV2605_ADDR);
        }

        // Exit standby: mode = 0x00 (internal trigger)
        let _ = i2c.write(DRV2605_ADDR, &[REG_MODE, 0x00]);

        // Select ERM library (library 1)
        let _ = i2c.write(DRV2605_ADDR, &[REG_LIBRARY, 0x01]);

        // Feedback control: ERM mode (bit 7 = 0)
        let mut fb = [0u8; 1];
        if i2c.write_read(DRV2605_ADDR, &[REG_FEEDBACK], &mut fb).is_ok() {
            let val = fb[0] & 0x7F; // Clear bit 7 for ERM
            let _ = i2c.write(DRV2605_ADDR, &[REG_FEEDBACK, val]);
        }

        // Increase drive strength: max out rated voltage and overdrive clamp
        let _ = i2c.write(DRV2605_ADDR, &[REG_RATED_VOLTAGE, 0xFF]);
        let _ = i2c.write(DRV2605_ADDR, &[REG_OD_CLAMP, 0xFF]);

        // Control3: set ERM open-loop (bit 5 = 1) for wider motor compatibility
        let mut c3 = [0u8; 1];
        if i2c.write_read(DRV2605_ADDR, &[REG_CONTROL3], &mut c3).is_ok() {
            let val = c3[0] | 0x20; // Set bit 5
            let _ = i2c.write(DRV2605_ADDR, &[REG_CONTROL3, val]);
        }

        Self { _private: () }
    }

    /// Fire a ROM effect from the built-in library.
    ///
    /// Loads the effect into waveform slot 1, terminates the sequence,
    /// and sets the GO bit. The motor runs autonomously — no need to wait.
    pub fn play(&self, i2c: &mut I2c<'_, Blocking>, effect: Effect) {
        // Waveform slot 1 = effect, slot 2 = 0 (end of sequence)
        let _ = i2c.write(DRV2605_ADDR, &[REG_WAVEFORM, effect as u8]);
        let _ = i2c.write(DRV2605_ADDR, &[REG_WAVEFORM + 1, 0x00]);
        // GO
        let _ = i2c.write(DRV2605_ADDR, &[REG_GO, 0x01]);
    }
}

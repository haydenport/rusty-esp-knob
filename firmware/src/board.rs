/// Pin assignments for the Waveshare ESP32-S3 1.8" Knob Display.
///
/// Reference: Waveshare wiki + BlueKnob community pin maps.

// --- Display (SH8601, QSPI AMOLED) ---
pub const LCD_QSPI_CLK: u8 = 13;
pub const LCD_QSPI_CS: u8 = 14;
pub const LCD_QSPI_DATA0: u8 = 15;
pub const LCD_QSPI_DATA1: u8 = 16;
pub const LCD_QSPI_DATA2: u8 = 17;
pub const LCD_QSPI_DATA3: u8 = 18;
pub const LCD_RST: u8 = 21;

// --- Backlight (PWM) ---
pub const BACKLIGHT: u8 = 47;

// --- Touch (CST816, I2C) ---
pub const TOUCH_SDA: u8 = 11;
pub const TOUCH_SCL: u8 = 12;
pub const TOUCH_INT: u8 = 9;
pub const TOUCH_RST: u8 = 10;

// --- Haptic motor (DRV2605, I2C — shares bus with touch) ---
pub const HAPTIC_I2C_ADDR: u8 = 0x5A;

// --- Rotary encoder ---
pub const ENCODER_A: u8 = 8;
pub const ENCODER_B: u8 = 7;

// --- USB OTG (CDC serial) ---
pub const USB_DP: u8 = 20;
pub const USB_DN: u8 = 19;

// --- I2S Audio DAC (PCM5100A) ---
pub const I2S_BCLK: u8 = 39;
pub const I2S_WS: u8 = 40;
pub const I2S_DOUT: u8 = 41;

// --- I2S PDM Microphone ---
pub const PDM_CLK: u8 = 45;
pub const PDM_DATA: u8 = 46;

// --- SD Card (SDMMC 4-bit) ---
pub const SD_D0: u8 = 5;
pub const SD_D1: u8 = 6;
pub const SD_D2: u8 = 42;
pub const SD_D3: u8 = 2;
pub const SD_CMD: u8 = 3;
pub const SD_CLK: u8 = 4;

// --- Display dimensions ---
pub const DISPLAY_WIDTH: u16 = 360;
pub const DISPLAY_HEIGHT: u16 = 360;

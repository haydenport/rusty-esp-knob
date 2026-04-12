#![no_std]
#![no_main]

mod board;

use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output};
use esp_hal::prelude::*;
use log::info;

#[entry]
fn main() -> ! {
    // Initialize esp-println logger
    esp_println::logger::init_logger_from_env();

    // Initialize peripherals with default config (160MHz CPU clock)
    let peripherals = esp_hal::init(esp_hal::Config::default());

    info!("Volume Knob Controller - firmware starting");
    info!("CPU initialized, configuring backlight on GPIO{}", board::BACKLIGHT);

    // Set up backlight pin as output, start high (on)
    let mut backlight = Output::new(peripherals.GPIO47, Level::High);

    let delay = Delay::new();

    info!("Entering blink loop - backlight should toggle every 500ms");

    let mut count: u32 = 0;
    loop {
        backlight.toggle();
        count += 1;
        info!("Blink #{count}");
        delay.delay_millis(500);
    }
}

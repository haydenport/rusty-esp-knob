#![no_std]
#![no_main]

extern crate alloc;

mod board;
mod encoder;
mod sh8601;

use esp_alloc as _;
use esp_backtrace as _;
esp_bootloader_esp_idf::esp_app_desc!();
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::main;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use log::info;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle};

use encoder::Encoder;
use sh8601::Sh8601;

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Initialize heap allocator for framebuffer (internal SRAM, 270KB)
    esp_alloc::heap_allocator!(size: 270_000);

    info!("Volume Knob Controller - firmware starting");

    // Backlight on
    let _backlight = Output::new(peripherals.GPIO47, Level::High, OutputConfig::default());

    // Display reset pin
    let rst = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());

    // Configure QSPI: Mode 0, 40 MHz
    let spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(40))
        .with_mode(Mode::_0);

    let spi = Spi::new(peripherals.SPI2, spi_config)
        .expect("SPI init failed")
        .with_sck(peripherals.GPIO13)
        .with_cs(peripherals.GPIO14)
        .with_sio0(peripherals.GPIO15)
        .with_sio1(peripherals.GPIO16)
        .with_sio2(peripherals.GPIO17)
        .with_sio3(peripherals.GPIO18);

    // DMA buffers: RX minimal, TX sized for framebuffer flush chunks
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(256, 32000);
    let dma_rx_buf = DmaRxBuf::new(rx_descriptors, rx_buffer).expect("DMA RX buf failed");
    let dma_tx_buf = DmaTxBuf::new(tx_descriptors, tx_buffer).expect("DMA TX buf failed");

    let spi_dma = spi.with_dma(peripherals.DMA_CH0).with_buffers(dma_rx_buf, dma_tx_buf);

    // Initialize display (allocates framebuffer on PSRAM)
    let mut display = Sh8601::new(spi_dma, rst);
    info!("Initializing display...");
    display.init().expect("Display init failed");
    info!("Display initialized");

    // Draw to framebuffer, then flush to display
    info!("Drawing test pattern...");
    display.clear(Rgb565::RED).unwrap();

    // White rectangle in the center
    Rectangle::new(Point::new(80, 80), Size::new(200, 200))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
        .draw(&mut display)
        .unwrap();

    // Blue circle on top
    Circle::new(Point::new(130, 130), 100)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLUE))
        .draw(&mut display)
        .unwrap();

    display.flush().expect("Flush failed");
    info!("Screen drawn");

    // Set up rotary encoder with software debounce (mirrors Waveshare C driver).
    let mut encoder = Encoder::new(peripherals.GPIO8, peripherals.GPIO7);
    info!("Encoder ready - turn the knob");

    let delay = Delay::new();
    let mut last_count: i32 = 0;
    loop {
        encoder.poll();
        let count = encoder.get();
        if count != last_count {
            info!("Encoder: {}", count);
            last_count = count;
        }
        delay.delay_millis(3);
    }
}

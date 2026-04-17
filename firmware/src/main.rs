#![no_std]
#![no_main]

extern crate alloc;

mod board;
mod encoder;
mod haptic;
mod sh8601;
mod touch;
mod usb_serial;

use esp_alloc as _;
use esp_backtrace as _;
esp_bootloader_esp_idf::esp_app_desc!();
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::main;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_hal::usb_serial_jtag::UsbSerialJtag;
use log::info;
use protocol::messages::{DeviceToHost, GestureKind, HostToDevice, PROTOCOL_VERSION};

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use u8g2_fonts::FontRenderer;
use u8g2_fonts::fonts::{u8g2_font_logisoso62_tn, u8g2_font_helvR24_tr};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

use encoder::Encoder;
use haptic::{Drv2605, Effect};
use sh8601::Sh8601;
use touch::Cst816;
use usb_serial::UsbSerial;

/// Convert a firmware touch gesture to the wire protocol enum.
fn to_wire_gesture(g: touch::Gesture) -> Option<GestureKind> {
    match g {
        touch::Gesture::SingleTap => Some(GestureKind::SingleTap),
        touch::Gesture::LongPress => Some(GestureKind::LongPress),
        touch::Gesture::SwipeUp => Some(GestureKind::SwipeUp),
        touch::Gesture::SwipeDown => Some(GestureKind::SwipeDown),
        touch::Gesture::SwipeLeft => Some(GestureKind::SwipeLeft),
        touch::Gesture::SwipeRight => Some(GestureKind::SwipeRight),
        touch::Gesture::DoubleTap | touch::Gesture::None => None,
    }
}

/// Draw the encoder count in the center of the screen.
fn draw_count(display: &mut Sh8601, count: i32) {
    use core::fmt::Write;
    Rectangle::new(Point::new(30, 110), Size::new(300, 130))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let mut buf = heapless::String::<16>::new();
    let _ = write!(buf, "{}", count);

    let font = FontRenderer::new::<u8g2_font_logisoso62_tn>();
    font.render_aligned(
        &*buf,
        Point::new(180, 180),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(Rgb565::WHITE),
        display,
    ).unwrap();
}

/// Draw the page index at the bottom of the screen.
fn draw_page(display: &mut Sh8601, page: i32) {
    use core::fmt::Write;
    Rectangle::new(Point::new(80, 290), Size::new(200, 40))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let mut buf = heapless::String::<24>::new();
    let _ = write!(buf, "Page {}", page);

    let font = FontRenderer::new::<u8g2_font_helvR24_tr>();
    font.render_aligned(
        &*buf,
        Point::new(180, 320),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(Rgb565::CSS_GRAY),
        display,
    ).unwrap();
}

/// Draw a status label at the top of the screen (e.g. "TAP", "LONG PRESS").
fn draw_status(display: &mut Sh8601, text: &str) {
    Rectangle::new(Point::new(80, 30), Size::new(200, 40))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let font = FontRenderer::new::<u8g2_font_helvR24_tr>();
    font.render_aligned(
        text,
        Point::new(180, 60),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(Rgb565::GREEN),
        display,
    ).unwrap();
}

#[main]
fn main() -> ! {
    // NOTE: esp-println logger is intentionally NOT initialized — it would
    // write log text to the same USB-Serial-JTAG peripheral that the protocol
    // codec uses, corrupting frames. Panics still emit a backtrace via
    // esp-backtrace (and those frames are corrupt on purpose — you want the trace).
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

    // Draw initial screen
    display.clear(Rgb565::BLACK).unwrap();
    draw_count(&mut display, 0);
    draw_page(&mut display, 0);
    display.flush().expect("Flush failed");
    info!("Screen drawn");

    // Set up rotary encoder with software debounce (mirrors Waveshare C driver).
    let mut encoder = Encoder::new(peripherals.GPIO8, peripherals.GPIO7);
    info!("Encoder ready - turn the knob");

    // Set up I2C bus shared by touch (CST816) and haptic (DRV2605)
    let i2c_config = I2cConfig::default().with_frequency(Rate::from_khz(400));
    let mut i2c = I2c::new(peripherals.I2C0, i2c_config)
        .expect("I2C init failed")
        .with_sda(peripherals.GPIO11)
        .with_scl(peripherals.GPIO12);

    let mut touch = Cst816::new(&mut i2c, peripherals.GPIO9, peripherals.GPIO10);
    info!("Touch ready");

    let haptic = Drv2605::init(&mut i2c);
    info!("Haptic ready");

    // USB Serial/JTAG for host communication
    let mut usb = UsbSerial::new(UsbSerialJtag::new(peripherals.USB_DEVICE));
    let _ = usb.send(&DeviceToHost::Ready { version: PROTOCOL_VERSION });
    info!("USB serial ready");

    let delay = Delay::new();
    let mut last_count: i32 = 0;
    let mut page: i32 = 0;
    let mut needs_flush = false;
    loop {
        encoder.poll();
        let count = encoder.get();
        if count != last_count {
            info!("Encoder: {}", count);
            haptic.play(&mut i2c, Effect::SharpClick);
            draw_count(&mut display, count);
            let _ = usb.send(&DeviceToHost::EncoderDelta(count - last_count));
            last_count = count;
            needs_flush = true;
        }

        if let Some(event) = touch.read(&mut i2c) {
            if event.gesture != touch::Gesture::None {
                info!("Touch: ({}, {}) gesture={:?}", event.x, event.y, event.gesture);
            }
            match event.gesture {
                touch::Gesture::SingleTap => {
                    haptic.play(&mut i2c, Effect::SharpClick);
                    draw_status(&mut display, "TAP");
                    needs_flush = true;
                }
                touch::Gesture::LongPress => {
                    haptic.play(&mut i2c, Effect::StrongClick);
                    draw_status(&mut display, "LONG PRESS");
                    needs_flush = true;
                }
                touch::Gesture::SwipeLeft => {
                    page -= 1;
                    info!("Page: {}", page);
                    draw_page(&mut display, page);
                    draw_status(&mut display, "< SWIPE");
                    needs_flush = true;
                }
                touch::Gesture::SwipeRight => {
                    page += 1;
                    info!("Page: {}", page);
                    draw_page(&mut display, page);
                    draw_status(&mut display, "SWIPE >");
                    needs_flush = true;
                }
                _ => {}
            }
            if let Some(kind) = to_wire_gesture(event.gesture) {
                let _ = usb.send(&DeviceToHost::Gesture(kind));
            }
        }

        // Handle incoming host messages (echo loop for Phase 4 testing)
        while let Some(msg) = usb.poll() {
            info!("USB RX: {:?}", msg);
            match msg {
                HostToDevice::Ping => {
                    let _ = usb.send(&DeviceToHost::Pong);
                }
                HostToDevice::Echo(data) => {
                    let _ = usb.send(&DeviceToHost::Echo(data));
                }
                _ => {}
            }
        }

        if needs_flush {
            display.flush().expect("Flush failed");
            needs_flush = false;
        }

        delay.delay_millis(3);
    }
}

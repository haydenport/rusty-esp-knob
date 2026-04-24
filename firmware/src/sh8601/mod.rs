mod init;

extern crate alloc;
use alloc::boxed::Box;

use embedded_graphics_core::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    pixelcolor::{raw::RawU16, Rgb565},
    prelude::*,
};
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Address, Command, DataMode, SpiDmaBus};
use esp_hal::spi::Error as SpiError;
use esp_hal::Blocking;

use crate::board;

/// QSPI opcode for register/command writes (single-wire data).
const CMD_WRITE: u16 = 0x02;
/// QSPI opcode for pixel/color writes (quad-wire data).
const CMD_WRITE_COLOR: u16 = 0x32;

/// MIPI DCS commands.
const COLMOD: u8 = 0x3A;
const CASET: u8 = 0x2A;
const RASET: u8 = 0x2B;
const RAMWR: u8 = 0x2C;

/// COLMOD value for RGB565 (16-bit color).
const COLMOD_RGB565: u8 = 0x55;

/// Framebuffer size in bytes: 360 × 360 × 2 (RGB565).
const FB_SIZE: usize = board::DISPLAY_WIDTH as usize * board::DISPLAY_HEIGHT as usize * 2;

/// Display driver for the Waveshare ESP32-S3 Knob Touch LCD 1.8" QSPI AMOLED.
///
/// Uses an in-memory framebuffer (heap-allocated). All drawing via
/// embedded-graphics writes to the framebuffer; call `flush()` to push the
/// framebuffer to the display over QSPI.
pub struct Sh8601<'d> {
    spi: SpiDmaBus<'d, Blocking>,
    rst: Output<'d>,
    delay: Delay,
    framebuffer: Box<[u8]>,
}

impl<'d> Sh8601<'d> {
    pub fn new(spi: SpiDmaBus<'d, Blocking>, rst: Output<'d>) -> Self {
        let framebuffer = alloc::vec![0u8; FB_SIZE].into_boxed_slice();
        Self {
            spi,
            rst,
            delay: Delay::new(),
            framebuffer,
        }
    }

    /// Initialize the display: hardware reset, COLMOD, vendor init sequence.
    pub fn init(&mut self) -> Result<(), SpiError> {
        // Hardware reset
        self.rst.set_high();
        self.delay.delay_millis(10);
        self.rst.set_low();
        self.delay.delay_millis(10);
        self.rst.set_high();
        self.delay.delay_millis(150);

        // Set COLMOD before vendor init
        self.write_register(COLMOD, &[COLMOD_RGB565])?;

        // Walk the full vendor init sequence — includes INVON, SLPOUT, DISPON, MADCTL
        for cmd in init::INIT_SEQUENCE {
            self.write_register(cmd.reg, cmd.data)?;
            if cmd.delay_ms > 0 {
                self.delay.delay_millis(cmd.delay_ms as u32);
            }
        }

        Ok(())
    }

    /// Flush the framebuffer to the display.
    ///
    /// Sets a full-screen window and streams the framebuffer in chunks
    /// using quad-mode pixel writes (opcode 0x32).
    pub fn flush(&mut self) -> Result<(), SpiError> {
        self.set_window(0, 0, board::DISPLAY_WIDTH - 1, board::DISPLAY_HEIGHT - 1)?;

        // Stream framebuffer in chunks sized to fit the DMA TX buffer.
        // Use per-chunk window rows instead of RAMWRC for reliability.
        const CHUNK_SIZE: usize = 30_000;
        const ROW_BYTES: usize = board::DISPLAY_WIDTH as usize * 2;
        // Round chunk to whole rows
        const ROWS_PER_CHUNK: usize = CHUNK_SIZE / ROW_BYTES;

        let mut row: u16 = 0;
        let mut offset = 0;

        while offset < self.framebuffer.len() {
            let rows_remaining = board::DISPLAY_HEIGHT - row;
            let rows = (ROWS_PER_CHUNK as u16).min(rows_remaining);
            let bytes = rows as usize * ROW_BYTES;

            self.set_window(0, row, board::DISPLAY_WIDTH - 1, row + rows - 1)?;

            // Write pixel data using quad-mode (opcode 0x32), accessing SPI
            // and framebuffer as separate fields to satisfy the borrow checker.
            let addr = (RAMWR as u32) << 8;
            self.spi.half_duplex_write(
                DataMode::Quad,
                Command::_8Bit(CMD_WRITE_COLOR, DataMode::Single),
                Address::_24Bit(addr, DataMode::Single),
                0,
                &self.framebuffer[offset..offset + bytes],
            )?;

            offset += bytes;
            row += rows;
        }

        Ok(())
    }

    /// Flush a horizontal band of rows to the display.
    ///
    /// Equivalent to `flush()` but limited to rows `y0..=y1`, reducing the
    /// amount of data sent over QSPI when only a small region changed.
    pub fn flush_rows(&mut self, y0: u16, y1: u16) -> Result<(), SpiError> {
        let y1 = y1.min(board::DISPLAY_HEIGHT - 1);
        if y0 > y1 {
            return Ok(());
        }

        const ROW_BYTES: usize = board::DISPLAY_WIDTH as usize * 2;
        const ROWS_PER_CHUNK: usize = 30_000 / ROW_BYTES;

        let mut row = y0;
        while row <= y1 {
            let rows = (ROWS_PER_CHUNK as u16).min(y1 + 1 - row);
            let bytes = rows as usize * ROW_BYTES;
            let offset = row as usize * ROW_BYTES;

            self.set_window(0, row, board::DISPLAY_WIDTH - 1, row + rows - 1)?;

            let addr = (RAMWR as u32) << 8;
            self.spi.half_duplex_write(
                DataMode::Quad,
                Command::_8Bit(CMD_WRITE_COLOR, DataMode::Single),
                Address::_24Bit(addr, DataMode::Single),
                0,
                &self.framebuffer[offset..offset + bytes],
            )?;

            row += rows;
        }
        Ok(())
    }

    /// Write a register command with parameter data (single-wire, opcode 0x02).
    fn write_register(&mut self, reg: u8, data: &[u8]) -> Result<(), SpiError> {
        let addr = (reg as u32) << 8;
        self.spi.half_duplex_write(
            DataMode::Single,
            Command::_8Bit(CMD_WRITE, DataMode::Single),
            Address::_24Bit(addr, DataMode::Single),
            0,
            data,
        )
    }

    /// Set the drawing window (column and row address range).
    fn set_window(&mut self, x0: u16, y0: u16, x1: u16, y1: u16) -> Result<(), SpiError> {
        self.write_register(
            CASET,
            &[
                (x0 >> 8) as u8,
                (x0 & 0xFF) as u8,
                (x1 >> 8) as u8,
                (x1 & 0xFF) as u8,
            ],
        )?;
        self.write_register(
            RASET,
            &[
                (y0 >> 8) as u8,
                (y0 & 0xFF) as u8,
                (y1 >> 8) as u8,
                (y1 & 0xFF) as u8,
            ],
        )
    }
}

/// embedded-graphics DrawTarget implementation.
///
/// All drawing writes to the in-memory framebuffer. Call `flush()` to update
/// the physical display.
impl DrawTarget for Sh8601<'_> {
    type Color = Rgb565;
    type Error = SpiError;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let w = board::DISPLAY_WIDTH as i32;
        let h = board::DISPLAY_HEIGHT as i32;
        let stride = board::DISPLAY_WIDTH as usize * 2;

        for Pixel(point, color) in pixels.into_iter() {
            let x = point.x;
            let y = point.y;
            if x < 0 || y < 0 || x >= w || y >= h {
                continue;
            }
            let idx = y as usize * stride + x as usize * 2;
            let raw = RawU16::from(color).into_inner();
            self.framebuffer[idx] = (raw >> 8) as u8;
            self.framebuffer[idx + 1] = (raw & 0xFF) as u8;
        }
        Ok(())
    }

    fn fill_contiguous<I>(
        &mut self,
        area: &embedded_graphics_core::primitives::Rectangle,
        colors: I,
    ) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        let area = area.intersection(&embedded_graphics_core::primitives::Rectangle::new(
            Point::zero(),
            Size::new(board::DISPLAY_WIDTH as u32, board::DISPLAY_HEIGHT as u32),
        ));

        if area.size.width == 0 || area.size.height == 0 {
            return Ok(());
        }

        let x0 = area.top_left.x as usize;
        let y0 = area.top_left.y as usize;
        let width = area.size.width as usize;
        let stride = board::DISPLAY_WIDTH as usize * 2;

        let height = area.size.height as usize;
        let mut col = 0usize;
        let mut row = 0usize;

        for color in colors.into_iter() {
            if row >= height {
                break;
            }
            let raw = RawU16::from(color).into_inner();
            let idx = (y0 + row) * stride + (x0 + col) * 2;
            self.framebuffer[idx] = (raw >> 8) as u8;
            self.framebuffer[idx + 1] = (raw & 0xFF) as u8;
            col += 1;
            if col >= width {
                col = 0;
                row += 1;
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        let raw = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = (raw & 0xFF) as u8;
        for pixel in self.framebuffer.chunks_exact_mut(2) {
            pixel[0] = hi;
            pixel[1] = lo;
        }
        Ok(())
    }
}

impl OriginDimensions for Sh8601<'_> {
    fn size(&self) -> Size {
        Size::new(board::DISPLAY_WIDTH as u32, board::DISPLAY_HEIGHT as u32)
    }
}

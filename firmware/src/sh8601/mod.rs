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

/// Bytes per display row (width × 2 for RGB565).
const ROW_BYTES: usize = board::DISPLAY_WIDTH as usize * 2;

/// Max bytes per QSPI write — must fit inside the DMA TX buffer configured
/// in `main.rs` (see `dma_buffers!(_, 32_000)`).
const CHUNK_SIZE: usize = 30_000;

/// Whole rows per chunk (rounded down so each chunk ends on a row boundary).
const ROWS_PER_CHUNK: usize = CHUNK_SIZE / ROW_BYTES;

/// DMA-safe copy buffer in internal SRAM. The framebuffer lives in PSRAM;
/// GDMA on this target cannot reliably source from PSRAM, so each chunk is
/// copied here before the half_duplex_write call.
static mut DMA_COPY_BUF: [u8; CHUNK_SIZE] = [0u8; CHUNK_SIZE];

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
        let mut row: u16 = 0;
        let mut offset = 0;

        while offset < self.framebuffer.len() {
            let rows_remaining = board::DISPLAY_HEIGHT - row;
            let rows = (ROWS_PER_CHUNK as u16).min(rows_remaining);
            let bytes = rows as usize * ROW_BYTES;

            self.set_window(0, row, board::DISPLAY_WIDTH - 1, row + rows - 1)?;

            // Copy to DMA-safe internal SRAM before writing. The framebuffer
            // is allocated in PSRAM and GDMA cannot reliably source from there.
            let addr = (RAMWR as u32) << 8;
            unsafe {
                DMA_COPY_BUF[..bytes].copy_from_slice(&self.framebuffer[offset..offset + bytes]);
                self.spi.half_duplex_write(
                    DataMode::Quad,
                    Command::_8Bit(CMD_WRITE_COLOR, DataMode::Single),
                    Address::_24Bit(addr, DataMode::Single),
                    0,
                    &DMA_COPY_BUF[..bytes],
                )?;
            }

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

        let mut row = y0;
        while row <= y1 {
            let rows = (ROWS_PER_CHUNK as u16).min(y1 + 1 - row);
            let bytes = rows as usize * ROW_BYTES;
            let offset = row as usize * ROW_BYTES;

            self.set_window(0, row, board::DISPLAY_WIDTH - 1, row + rows - 1)?;

            let addr = (RAMWR as u32) << 8;
            unsafe {
                DMA_COPY_BUF[..bytes].copy_from_slice(&self.framebuffer[offset..offset + bytes]);
                self.spi.half_duplex_write(
                    DataMode::Quad,
                    Command::_8Bit(CMD_WRITE_COLOR, DataMode::Single),
                    Address::_24Bit(addr, DataMode::Single),
                    0,
                    &DMA_COPY_BUF[..bytes],
                )?;
            }

            row += rows;
        }
        Ok(())
    }

    /// Fill the full 360° annular ring [inner_r, outer_r] at center (cx, cy).
    ///
    /// No angular check — faster than a 270° arc wipe and ensures the cap
    /// circles at the arc tips (which straddle the angular boundaries) are
    /// fully cleared along with the rest of the band.
    pub fn fill_ring(
        &mut self,
        cx: i32, cy: i32,
        inner_r: i32, outer_r: i32,
        color: Rgb565,
    ) {
        let raw = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = (raw & 0xFF) as u8;
        let w = board::DISPLAY_WIDTH as i32;
        let h = board::DISPLAY_HEIGHT as i32;
        let inner_r2 = inner_r * inner_r;
        let outer_r2 = outer_r * outer_r;

        for dy in -outer_r..=outer_r {
            let dy2 = dy * dy;
            if dy2 > outer_r2 { continue; }
            let y = cy + dy;
            if y < 0 || y >= h { continue; }
            let base = y as usize * ROW_BYTES;

            let x_outer = libm::sqrtf((outer_r2 - dy2) as f32) as i32;

            if dy2 >= inner_r2 {
                let x0 = (cx - x_outer).max(0) as usize;
                let x1 = (cx + x_outer).min(w - 1) as usize;
                for x in x0..=x1 {
                    self.framebuffer[base + x * 2] = hi;
                    self.framebuffer[base + x * 2 + 1] = lo;
                }
            } else {
                let x_inner = libm::sqrtf((inner_r2 - dy2) as f32) as i32;
                let lx0 = (cx - x_outer).max(0) as usize;
                let lx1 = (cx - x_inner).min(w - 1) as usize;
                let rx0 = (cx + x_inner).max(0) as usize;
                let rx1 = (cx + x_outer).min(w - 1) as usize;
                for x in lx0..=lx1 {
                    self.framebuffer[base + x * 2] = hi;
                    self.framebuffer[base + x * 2 + 1] = lo;
                }
                for x in rx0..=rx1 {
                    self.framebuffer[base + x * 2] = hi;
                    self.framebuffer[base + x * 2 + 1] = lo;
                }
            }
        }
    }

    /// Fill the 270° arc ring (start=135°, gap at bottom between 45°–135°).
    ///
    /// Angle test: exclude pixels where `dy > dx.abs()` (the excluded wedge
    /// around 6-o'clock). This is an exact integer check — no trigonometry.
    pub fn fill_ring_270(
        &mut self,
        cx: i32, cy: i32,
        inner_r: i32, outer_r: i32,
        color: Rgb565,
    ) {
        let raw = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = (raw & 0xFF) as u8;
        let w = board::DISPLAY_WIDTH as i32;
        let h = board::DISPLAY_HEIGHT as i32;
        let inner_r2 = inner_r * inner_r;
        let outer_r2 = outer_r * outer_r;

        for dy in -outer_r..=outer_r {
            let dy2 = dy * dy;
            if dy2 > outer_r2 { continue; }
            let y = cy + dy;
            if y < 0 || y >= h { continue; }
            let base = y as usize * ROW_BYTES;

            let x_outer = libm::sqrtf((outer_r2 - dy2) as f32) as i32;

            // Row-level skip: if every pixel on this row is inside the excluded
            // bottom wedge (max |dx| = x_outer < dy), skip the whole row.
            if dy > x_outer { continue; }

            if dy2 >= inner_r2 {
                let x0 = (cx - x_outer).max(0);
                let x1 = (cx + x_outer).min(w - 1);
                for xi in x0..=x1 {
                    let dx = xi - cx;
                    if dy > dx.abs() { continue; }
                    let x = xi as usize;
                    self.framebuffer[base + x * 2] = hi;
                    self.framebuffer[base + x * 2 + 1] = lo;
                }
            } else {
                let x_inner = libm::sqrtf((inner_r2 - dy2) as f32) as i32;
                for &(x0, x1) in &[
                    ((cx - x_outer).max(0), (cx - x_inner).min(w - 1)),
                    ((cx + x_inner).max(0), (cx + x_outer).min(w - 1)),
                ] {
                    for xi in x0..=x1 {
                        let dx = xi - cx;
                        if dy > dx.abs() { continue; }
                        let x = xi as usize;
                        self.framebuffer[base + x * 2] = hi;
                        self.framebuffer[base + x * 2 + 1] = lo;
                    }
                }
            }
        }
    }

    /// Fill a partial arc [135°, 135°+sweep_deg] of the ring [inner_r, outer_r].
    ///
    /// Used for the white volume-fill portion. Uses two cross-product half-plane
    /// tests (precomputed per call, integer+float per pixel — no atan2).
    /// For sweep_deg ≤ 180° uses AND logic; > 180° uses OR logic to handle
    /// the case where the fill arc wraps past the 180° half-plane boundary.
    pub fn fill_ring_arc(
        &mut self,
        cx: i32, cy: i32,
        inner_r: i32, outer_r: i32,
        sweep_deg: f32,
        color: Rgb565,
    ) {
        let raw = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = (raw & 0xFF) as u8;
        let w = board::DISPLAY_WIDTH as i32;
        let h = board::DISPLAY_HEIGHT as i32;
        let inner_r2 = inner_r * inner_r;
        let outer_r2 = outer_r * outer_r;

        // Precompute end direction B = (cos(135°+sweep°), sin(135°+sweep°)).
        let end_rad = (135.0_f32 + sweep_deg) * core::f32::consts::PI / 180.0;
        let bx = libm::cosf(end_rad);
        let by = libm::sinf(end_rad);
        let use_or = sweep_deg > 180.0;

        for dy in -outer_r..=outer_r {
            let dy2 = dy * dy;
            if dy2 > outer_r2 { continue; }
            let y = cy + dy;
            if y < 0 || y >= h { continue; }
            let base = y as usize * ROW_BYTES;
            let dy_f = dy as f32;

            let x_outer = libm::sqrtf((outer_r2 - dy2) as f32) as i32;

            if dy2 >= inner_r2 {
                let x0 = (cx - x_outer).max(0);
                let x1 = (cx + x_outer).min(w - 1);
                for xi in x0..=x1 {
                    let dx = xi - cx;
                    // Start check: cross(A=(cos135°,sin135°), P) >= 0
                    // ⟺ -(dx+dy)/√2 >= 0 ⟺ dx+dy <= 0
                    let after_start = dx + dy <= 0;
                    // End check: cross(P, B) >= 0 ⟺ dx*by - dy*bx >= 0
                    let before_end = dx as f32 * by - dy_f * bx >= 0.0;
                    let in_fill = if use_or { after_start || before_end }
                                  else      { after_start && before_end };
                    if in_fill {
                        let x = xi as usize;
                        self.framebuffer[base + x * 2] = hi;
                        self.framebuffer[base + x * 2 + 1] = lo;
                    }
                }
            } else {
                let x_inner = libm::sqrtf((inner_r2 - dy2) as f32) as i32;
                for &(x0, x1) in &[
                    ((cx - x_outer).max(0), (cx - x_inner).min(w - 1)),
                    ((cx + x_inner).max(0), (cx + x_outer).min(w - 1)),
                ] {
                    for xi in x0..=x1 {
                        let dx = xi - cx;
                        let after_start = dx + dy <= 0;
                        let before_end = dx as f32 * by - dy_f * bx >= 0.0;
                        let in_fill = if use_or { after_start || before_end }
                                      else      { after_start && before_end };
                        if in_fill {
                            let x = xi as usize;
                            self.framebuffer[base + x * 2] = hi;
                            self.framebuffer[base + x * 2 + 1] = lo;
                        }
                    }
                }
            }
        }
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

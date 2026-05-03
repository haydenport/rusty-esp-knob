#![no_std]
#![no_main]

extern crate alloc;

mod backlight;
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
use esp_hal::gpio::{Io, Level, Output, OutputConfig};
use backlight::Backlight;
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
use embedded_graphics::primitives::{Arc, Circle, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, StrokeAlignment};
use u8g2_fonts::FontRenderer;
use u8g2_fonts::fonts::u8g2_font_helvR24_tr;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

use alloc::vec::Vec;

use encoder::Encoder;
use haptic::{Drv2605, Effect};
use protocol::messages::AppInfo;
use sh8601::Sh8601;
use touch::Cst816;
use usb_serial::UsbSerial;

const ICON_SIZE: u32 = 64;
const ICON_X: i32 = (360 - ICON_SIZE as i32) / 2;
const ICON_Y: i32 = 60;

/// Redraw the icon, name, and volume for the currently-selected app.
fn redraw_app(
    display: &mut Sh8601,
    apps: &[AppInfo],
    selected: Option<u32>,
    icon: &[u8],
    volume: u8,
) {
    draw_app_icon(display, icon);
    let name = selected
        .and_then(|id| apps.iter().find(|a| a.id == id))
        .map(|a| a.name.as_str())
        .unwrap_or("(no app)");
    draw_app_name(display, name);
    draw_volume(display, volume);
}

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

/// Draw a circular arc bar on the outer edge showing `volume` (0–100).
/// A 270° track (lower-left → over the top → lower-right) is drawn in dim gray;
/// the filled portion grows clockwise from lower-left proportional to volume.
/// Filled circles are placed at each cap to give a rounded end-cap appearance.
fn draw_volume(display: &mut Sh8601, volume: u8) {
    // Arc center = (180, 180). top_left = center - radius = (10, 10), diameter = 340.
    // Stroke 14 px → cap circles have the same diameter so they blend seamlessly.
    const TOP_LEFT: Point = Point::new(10, 10);
    const DIAMETER: u32 = 340;
    const STROKE: u32 = 14;
    const START_DEG: f32 = 135.0; // 7:30 o'clock (lower-left)
    const SWEEP_DEG: f32 = 270.0; // ends at 4:30 o'clock (lower-right)

    // Pre-computed cap centres for the fixed track endpoints.
    // 135°: x = 180 + 170*cos(135°) = 180 - 120.2 ≈ 60, y = 180 + 170*sin(135°) ≈ 300
    const CAP_START: Point = Point::new(60, 300);
    // 405°=45°: x = 180 + 170*cos(45°) ≈ 300, y ≈ 300
    const CAP_END: Point = Point::new(300, 300);

    // Wipe the bar's annular footprint to black before drawing. The Arc and
    // Circle primitives in embedded-graphics each make slightly different
    // edge-pixel decisions (different center_2x, different inclusion paths
    // through PlaneSector for Intersection vs Union sweeps), so a stale
    // white pixel from a previous frame can survive both the gray arc's
    // overdraw and the new white arc's overdraw — appearing as a bright
    // sliver along the inner edge. Wiping with a wider stroke than the
    // visible bar guarantees nothing from the prior frame survives.
    //
    // Wipe stroke 32 covers radii 154–186 (centerline 170 ± 16), which
    // brackets the bar's 163–177 range plus the cap circles that extend
    // ~7 px past the centerline at the tip.
    Arc::new(TOP_LEFT, DIAMETER, Angle::from_degrees(START_DEG), Angle::from_degrees(SWEEP_DEG))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::BLACK, 32))
        .draw(display)
        .unwrap();
    // Wipe also covers the cap circles' bounding regions at the fixed
    // endpoints — they sit at the start/end of the 270° sweep, but the
    // Arc primitive's inclusion test may not reach the very corners of
    // those cap pixels at the angular boundaries.
    draw_arc_cap(display, CAP_START, 32, Rgb565::BLACK);
    draw_arc_cap(display, CAP_END, 32, Rgb565::BLACK);

    // Full track in dim gray, then round caps at both track endpoints.
    Arc::new(TOP_LEFT, DIAMETER, Angle::from_degrees(START_DEG), Angle::from_degrees(SWEEP_DEG))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_DIM_GRAY, STROKE))
        .draw(display)
        .unwrap();
    draw_arc_cap(display, CAP_START, STROKE, Rgb565::CSS_DIM_GRAY);
    draw_arc_cap(display, CAP_END, STROKE, Rgb565::CSS_DIM_GRAY);

    // Filled portion proportional to volume.
    let fill_sweep = SWEEP_DEG * (volume as f32 / 100.0);
    if fill_sweep >= 1.0 {
        Arc::new(
            TOP_LEFT,
            DIAMETER,
            Angle::from_degrees(START_DEG),
            Angle::from_degrees(fill_sweep),
        )
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, STROKE))
        .draw(display)
        .unwrap();

        // White cap at fill start (overlays the gray one).
        draw_arc_cap(display, CAP_START, STROKE, Rgb565::WHITE);

        // White cap at fill tip — position computed at runtime from the sweep angle.
        draw_arc_cap(display, arc_tip_point(fill_sweep), STROKE, Rgb565::WHITE);
    }
}

/// Draw a filled circle centred on `centre` with the given `diameter` (= arc stroke width).
fn draw_arc_cap(display: &mut Sh8601, centre: Point, diameter: u32, color: Rgb565) {
    let r = (diameter / 2) as i32;
    Circle::new(Point::new(centre.x - r, centre.y - r), diameter)
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(display)
        .unwrap();
}

/// Compute the point on the arc path at `sweep_deg` degrees from the start.
/// Arc center = (180,180), radius = 170, start angle = 135°.
fn arc_tip_point(sweep_deg: f32) -> Point {
    let rad = (135.0_f32 + sweep_deg) * (core::f32::consts::PI / 180.0);
    Point::new(
        180 + (170.0_f32 * libm::cosf(rad)) as i32,
        180 + (170.0_f32 * libm::sinf(rad)) as i32,
    )
}

/// Redraw the whole volume bar into the framebuffer and return the `(y0, y1)`
/// row range that actually changed, suitable for passing to `display.flush_rows`.
///
/// We always redraw the full bar (not just the angular delta) because the arc
/// rasterizer in embedded-graphics makes slightly different inner-edge pixel
/// decisions for `Arc(start, sweep)` calls with different parameters covering
/// the same physical angle range. Painting only a small "vacated" segment in
/// gray would leave 1–2 px white slivers along the inner edge that the gray
/// arc didn't claim — visible as bright ragged spots after rotating up then
/// back down. Full redraw keeps the framebuffer self-consistent; the partial
/// flush is what makes this cheap.
fn draw_volume_delta(display: &mut Sh8601, prev_vol: u8, new_vol: u8) -> (u16, u16) {
    if prev_vol == new_vol {
        return (0, 0);
    }

    const STROKE: u32 = 14;
    const FULL_SWEEP: f32 = 270.0;
    const CAP_START: Point = Point::new(60, 300);

    draw_volume(display, new_vol);

    // Compute dirty row bounds. The framebuffer is now consistent everywhere,
    // but only rows touching the changed angular range differ from the previous
    // frame, so we flush just those.
    let prev_sweep = FULL_SWEEP * (prev_vol as f32 / 100.0);
    let new_sweep  = FULL_SWEEP * (new_vol  as f32 / 100.0);

    let cap_r = (STROKE / 2 + 1) as i32;
    let p_old = if prev_vol > 0 { arc_tip_point(prev_sweep) } else { CAP_START };
    let p_new = if new_vol > 0 { arc_tip_point(new_sweep) } else { CAP_START };

    let mut y_min = p_old.y.min(p_new.y) - cap_r;
    let mut y_max = p_old.y.max(p_new.y) + cap_r;

    // Include start cap row range when it changes colour.
    if prev_vol == 0 || new_vol == 0 {
        y_min = y_min.min(CAP_START.y - cap_r);
        y_max = y_max.max(CAP_START.y + cap_r);
    }

    let lo = prev_sweep.min(new_sweep);
    let hi = prev_sweep.max(new_sweep);

    // Arc top (sweep = 135° → y = 10).
    if lo <= 135.0 && hi >= 135.0 {
        y_min = y_min.min(10 - cap_r);
    }
    // Arc left/right mid-points (sweep = 45° and 225° → y = 180).
    for special in [45.0_f32, 225.0_f32] {
        if lo <= special && hi >= special {
            y_min = y_min.min(180 - cap_r);
            y_max = y_max.max(180 + cap_r);
        }
    }

    (y_min.max(0) as u16, y_max.min(359) as u16)
}

/// Show or clear a "MUTED" label just below the volume area (y=248).
fn draw_mute_indicator(display: &mut Sh8601, muted: bool) {
    draw_status(display, if muted { "MUTED" } else { "" });
}

/// Full-screen "waiting for companion" state. Shown at boot and after the
/// companion disconnects. Clears the screen and draws a "NOT DETECTED" label
/// in the centre. The arc on the outer edge is drawn by the subsequent
/// draw_volume call so the standalone encoder value is visible.
fn draw_not_connected(display: &mut Sh8601) {
    display.clear(Rgb565::BLACK).unwrap();
    let font = FontRenderer::new::<u8g2_font_helvR24_tr>();
    font.render_aligned(
        "NOT DETECTED",
        Point::new(180, 200),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(Rgb565::YELLOW),
        display,
    ).unwrap();
}

/// Blit a raw RGB565 image (big-endian, width×height pixels) to the display.
fn draw_icon(display: &mut Sh8601, pixels: &[u8], width: u32, height: u32, origin: Point) {
    use embedded_graphics::image::{Image, ImageRaw};
    use embedded_graphics::pixelcolor::raw::BigEndian;

    let raw = ImageRaw::<Rgb565, BigEndian>::new(pixels, width);
    let _ = Image::new(&raw, origin).draw(display);
}

/// Clear the icon region and paint the given RGB565 bytes there. If `pixels`
/// is empty, draw a placeholder rectangle instead.
fn draw_app_icon(display: &mut Sh8601, pixels: &[u8]) {
    Rectangle::new(Point::new(ICON_X, ICON_Y), Size::new(ICON_SIZE, ICON_SIZE))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let expected = (ICON_SIZE * ICON_SIZE * 2) as usize;
    if pixels.len() == expected {
        draw_icon(display, pixels, ICON_SIZE, ICON_SIZE, Point::new(ICON_X, ICON_Y));
    } else {
        // Placeholder: hollow outline. `StrokeAlignment::Inside` keeps the
        // 2 px stroke entirely within the rect so the black fill we just
        // drew covers every pixel the next icon blit will touch — without
        // this, the outer edge of a centered stroke leaks 1 px outside the
        // fill and lingers as a ghost box behind the real icon.
        let style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb565::CSS_DIM_GRAY)
            .stroke_width(2)
            .stroke_alignment(StrokeAlignment::Inside)
            .build();
        Rectangle::new(Point::new(ICON_X, ICON_Y), Size::new(ICON_SIZE, ICON_SIZE))
            .into_styled(style)
            .draw(display)
            .unwrap();
    }
}

/// Draw the selected app's name directly below the icon.
fn draw_app_name(display: &mut Sh8601, name: &str) {
    Rectangle::new(Point::new(20, ICON_Y + ICON_SIZE as i32 + 10), Size::new(320, 40))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let font = FontRenderer::new::<u8g2_font_helvR24_tr>();
    let _ = font.render_aligned(
        name,
        Point::new(180, ICON_Y + ICON_SIZE as i32 + 36),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(Rgb565::WHITE),
        display,
    );
}

/// Debug readout of the USB-RX stats at the bottom of the screen. Format:
/// `t=<tick> b=<bytes> ok=<decoded> er=<errs> of=<overflows>`. The `t` counter
/// increments every main-loop iteration — if it stops climbing on-screen, the
/// main loop has hard-locked.
fn draw_rx_stats(display: &mut Sh8601, stats: usb_serial::RxStats, tick: u32) {
    use core::fmt::Write;
    use u8g2_fonts::fonts::u8g2_font_profont15_tr;

    Rectangle::new(Point::new(0, 248), Size::new(360, 22))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let free = esp_alloc::HEAP.free();
    let mut buf: heapless::String<96> = heapless::String::new();
    let _ = write!(
        buf,
        "t={} b={} ok={} of={} h={} dr={}",
        tick, stats.bytes, stats.ok, stats.overflow, free, stats.tx_drop
    );

    let font = FontRenderer::new::<u8g2_font_profont15_tr>();
    let _ = font.render_aligned(
        buf.as_str(),
        Point::new(180, 264),
        VerticalPosition::Baseline,
        HorizontalAlignment::Center,
        FontColor::Transparent(Rgb565::CSS_LIGHT_GRAY),
        display,
    );
}

/// Draw a status label in the centre band (e.g. "MUTED", "LONG PRESS").
/// Also clears the "NOT DETECTED" text that lives in the same region so
/// passing an empty string cleanly erases any previous status.
fn draw_status(display: &mut Sh8601, text: &str) {
    Rectangle::new(Point::new(30, 158), Size::new(300, 90))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    let font = FontRenderer::new::<u8g2_font_helvR24_tr>();
    font.render_aligned(
        text,
        Point::new(180, 210),
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

    // Initialize heap allocator for framebuffer + protocol buffers (internal SRAM).
    // Framebuffer is ~253 KB; the rest covers decoder, icon, and app-list allocations.
    // ~295 KB is the ceiling before the heap collides with the stack region.
    esp_alloc::heap_allocator!(size: 290_000);


    info!("Volume Knob Controller - firmware starting");

    // PWM backlight with idle auto-dim/off. Driven from the main loop via
    // notify_activity() on user input and host-driven volume/mute changes.
    let mut backlight = Backlight::new(peripherals.LEDC, peripherals.GPIO47);

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

    // Draw initial screen — companion not yet connected.
    draw_not_connected(&mut display);
    draw_volume(&mut display, 50);
    display.flush().expect("Flush failed");
    info!("Screen drawn");

    // Set up rotary encoder — interrupt-driven, counts in GPIO ISR.
    let mut io = Io::new(peripherals.IO_MUX);
    let mut encoder = Encoder::new(peripherals.GPIO8, peripherals.GPIO7, &mut io);
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

    // USB Serial/JTAG for host communication.
    // Ready is sent lazily on first received host message so it isn't
    // transmitted during USB re-enumeration (which happens right after
    // flashing). Bytes sent before the host CDC driver is ready land in
    // the FIFO at an unpredictable time and corrupt subsequent Ack frames.
    let mut usb = UsbSerial::new(UsbSerialJtag::new(peripherals.USB_DEVICE));
    let mut ready_sent = false;
    info!("USB serial ready");

    let delay = Delay::new();
    let mut last_count: i32 = 0;
    let mut last_encoder_tick: u32 = 0;
    let mut needs_flush = false;

    // App-list state pushed by the host via SetAppList / SetSelectedApp.
    let mut apps: Vec<AppInfo> = Vec::new();
    let mut selected_app: Option<u32> = None;
    // Index into `apps` for carousel navigation.
    let mut selected_idx: usize = 0;
    // Icon for the currently-selected app (RGB565). Cleared on app switch;
    // repopulated when the companion pushes SetAppIcon in response to AppSelected.
    let mut current_icon: Vec<u8> = Vec::new();
    let mut current_volume: u8 = 0;
    let mut current_muted: bool = false;
    let mut loop_tick: u32 = 0;
    // Value shown by the encoder when no companion is connected (0-100).
    let mut standalone_value: u8 = 50;
    // Tick at which the last host message arrived. Used to detect companion
    // disconnect: if no message for ~3 000 ticks (~9 s at 3 ms/tick) while
    // apps are loaded, we clear the app list and return to the not-connected
    // screen. The companion sends heartbeat Pings every 5 s so this fires
    // roughly two missed heartbeats after a real disconnect.
    let mut last_host_msg_tick: u32 = 0;
    // Debug overlay (RX stats) is off by default; hold finger for 4 s to toggle.
    let mut debug_visible: bool = false;
    // Counts consecutive ticks where the touch IRQ is asserted (finger held down).
    let mut touch_held_ticks: u32 = 0;
    loop {
        loop_tick = loop_tick.wrapping_add(1);
        encoder.poll();
        let count = encoder.get();
        if count != last_count {
            let raw_delta = count - last_count;
            let delta = raw_delta.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            last_encoder_tick = loop_tick;
            backlight.notify_activity(loop_tick);

            if let Some(app_id) = selected_app {
                let _ = usb.send(&DeviceToHost::VolumeDelta { app_id, delta });
            } else {
                let prev_vol = standalone_value;
                standalone_value = (standalone_value as i16 + delta as i16)
                    .clamp(0, 100) as u8;
                let (dy0, dy1) = draw_volume_delta(&mut display, prev_vol, standalone_value);
                display.flush_rows(dy0, dy1).expect("Flush failed");
            }
            last_count = count;
        }

        // Only read touch registers when the IRQ pin is asserted or a finger
        // was already down. Tap/long-press gestures fire at finger-lift when
        // IRQ is already high, so the `is_finger_down()` arm catches those.
        let touch_event = if touch.irq_asserted() || touch.is_finger_down() {
            touch.read(&mut i2c)
        } else {
            None
        };

        // 4-second hold → toggle debug stats overlay.
        // Uses `is_finger_down()` (num_points register) rather than the IRQ
        // pin, which only pulses briefly and is already high by finger-lift.
        if touch.is_finger_down() {
            // Treat sustained contact as activity so a long-press doesn't dim.
            backlight.notify_activity(loop_tick);
            touch_held_ticks = touch_held_ticks.saturating_add(1);
            if touch_held_ticks == 1_334 { // 1 334 ticks × 3 ms ≈ 4 s
                debug_visible = !debug_visible;
                // Full redraw so the stats band (which overlaps the volume arc)
                // is cleanly removed or shown without any black bars.
                if apps.is_empty() {
                    draw_not_connected(&mut display);
                    draw_volume(&mut display, standalone_value);
                } else {
                    display.clear(Rgb565::BLACK).unwrap();
                    redraw_app(&mut display, &apps, selected_app, &current_icon, current_volume);
                    draw_mute_indicator(&mut display, current_muted);
                }
                needs_flush = true;
            }
        } else {
            touch_held_ticks = 0;
        }

        if let Some(event) = touch_event {
            if event.gesture != touch::Gesture::None {
                info!("Touch: ({}, {}) gesture={:?}", event.x, event.y, event.gesture);
            }
            // If the screen was fully off, the first touch only wakes it —
            // don't fire the gesture (otherwise a wake-tap would also toggle
            // mute on whatever app happened to be selected).
            if backlight.wake_from_off(loop_tick) {
                continue;
            }
            match event.gesture {
                touch::Gesture::SingleTap => {
                    // Tap toggles mute for the selected app. Companion will
                    // set the WASAPI mute and echo back SetMute so we can
                    // update the display.
                    if let Some(id) = selected_app {
                        haptic.play(&mut i2c, Effect::SharpClick);
                        let _ = usb.send(&DeviceToHost::MuteToggle { app_id: id });
                    }
                }
                touch::Gesture::LongPress => {
                    haptic.play(&mut i2c, Effect::StrongClick);
                    draw_status(&mut display, "LONG PRESS");
                    needs_flush = true;
                }
                touch::Gesture::SwipeLeft => {
                    if !apps.is_empty() {
                        selected_idx = (selected_idx + apps.len() - 1) % apps.len();
                        let app = &apps[selected_idx];
                        selected_app = Some(app.id);
                        current_volume = app.volume;
                        current_muted = app.muted;
                        // Drop the old icon's heap capacity (not just its
                        // contents) — the next SetAppIcon will allocate ~8 KB
                        // for the new pixels while still reading bytes into
                        // the decoder buffer, so we need the headroom.
                        current_icon = Vec::new();
                        haptic.play(&mut i2c, Effect::SharpClick);
                        redraw_app(&mut display, &apps, selected_app, &[], current_volume);
                        draw_mute_indicator(&mut display, current_muted);
                        let _ = usb.send(&DeviceToHost::AppSelected(app.id));
                        needs_flush = true;
                    }
                }
                touch::Gesture::SwipeRight => {
                    if !apps.is_empty() {
                        selected_idx = (selected_idx + 1) % apps.len();
                        let app = &apps[selected_idx];
                        selected_app = Some(app.id);
                        current_volume = app.volume;
                        current_muted = app.muted;
                        // Drop the old icon's heap capacity (not just its
                        // contents) — the next SetAppIcon will allocate ~8 KB
                        // for the new pixels while still reading bytes into
                        // the decoder buffer, so we need the headroom.
                        current_icon = Vec::new();
                        haptic.play(&mut i2c, Effect::SharpClick);
                        redraw_app(&mut display, &apps, selected_app, &[], current_volume);
                        draw_mute_indicator(&mut display, current_muted);
                        let _ = usb.send(&DeviceToHost::AppSelected(app.id));
                        needs_flush = true;
                    }
                }
                _ => {}
            }
            if let Some(kind) = to_wire_gesture(event.gesture) {
                let _ = usb.send(&DeviceToHost::Gesture(kind));
            }
        }

        // Handle incoming host messages. Each message gets an `Ack` reply so
        // the companion can flow-control large writes (icon pushes) — the PC
        // waits for the Ack before sending the next message.
        while let Some(msg) = usb.poll() {
            last_host_msg_tick = loop_tick;
            if !ready_sent {
                let _ = usb.send(&DeviceToHost::Ready { version: PROTOCOL_VERSION });
                ready_sent = true;
            }
            let mut ack = true;
            match msg {
                HostToDevice::Ping => {
                    ack = false; // Pong replaces the ack for ping.
                    let _ = usb.send(&DeviceToHost::Pong);
                }
                HostToDevice::Echo(data) => {
                    ack = false; // Echo reply replaces the ack.
                    let _ = usb.send(&DeviceToHost::Echo(data));
                }
                HostToDevice::SetBacklight { active_pct, dim_after_secs, off_after_secs } => {
                    backlight.set_active_pct(active_pct);
                    backlight.set_timeouts(dim_after_secs, off_after_secs);
                }
                HostToDevice::SetAppList(list) => {
                    backlight.notify_activity(loop_tick);
                    apps = list;
                    if selected_app.is_none() {
                        selected_app = apps.first().map(|a| a.id);
                        selected_idx = 0;
                    } else {
                        selected_idx = apps.iter().position(|a| Some(a.id) == selected_app).unwrap_or(0);
                    }
                    if let Some(id) = selected_app {
                        if let Some(app) = apps.iter().find(|a| a.id == id) {
                            current_volume = app.volume;
                            current_muted = app.muted;
                        }
                    }
                    redraw_app(&mut display, &apps, selected_app, &current_icon, current_volume);
                    draw_mute_indicator(&mut display, current_muted);
                    needs_flush = true;
                }
                HostToDevice::SetAppIcon { app_id, pixels } => {
                    if selected_app == Some(app_id) {
                        current_icon = pixels;
                        draw_app_icon(&mut display, &current_icon);
                        needs_flush = true;
                    }
                }
                HostToDevice::SetSelectedApp(id) => {
                    backlight.notify_activity(loop_tick);
                    selected_app = Some(id);
                    selected_idx = apps.iter().position(|a| a.id == id).unwrap_or(0);
                    // Free old icon capacity — see swipe handler for why.
                    current_icon = Vec::new();
                    if let Some(app) = apps.iter().find(|a| a.id == id) {
                        current_volume = app.volume;
                        current_muted = app.muted;
                    }
                    redraw_app(&mut display, &apps, selected_app, &current_icon, current_volume);
                    draw_mute_indicator(&mut display, current_muted);
                    needs_flush = true;
                }
                HostToDevice::SetVolume { app_id, level } => {
                    // OS-mixer or other-app volume change is user activity too.
                    backlight.notify_activity(loop_tick);
                    // Update the local app list so stale volumes aren't shown after a swipe.
                    if let Some(app) = apps.iter_mut().find(|a| a.id == app_id) {
                        app.volume = level;
                    }
                    if selected_app == Some(app_id) {
                        let prev_vol = current_volume;
                        current_volume = level;
                        let (dy0, dy1) = draw_volume_delta(&mut display, prev_vol, current_volume);
                        display.flush_rows(dy0, dy1).expect("Flush failed");
                        needs_flush = false;
                    }
                }
                HostToDevice::SetMute { app_id, muted } => {
                    backlight.notify_activity(loop_tick);
                    if let Some(app) = apps.iter_mut().find(|a| a.id == app_id) {
                        app.muted = muted;
                    }
                    if selected_app == Some(app_id) {
                        current_muted = muted;
                        haptic.play(&mut i2c, Effect::SharpClick);
                        draw_mute_indicator(&mut display, current_muted);
                        needs_flush = true;
                    }
                }
            }
            // Flush now so the host sees `Ack` only after the display work
            // for this message is actually done.
            if needs_flush {
                display.flush().expect("Flush failed");
                needs_flush = false;
            }
            if ack {
                let _ = usb.send(&DeviceToHost::Ack);
            }
        }

        // Detect companion disconnect: ~9 s of silence (3 000 ticks × 3 ms)
        // while an app list is loaded means the companion has gone away.
        if !apps.is_empty() && loop_tick.wrapping_sub(last_host_msg_tick) > 3_000 {
            apps.clear();
            selected_app = None;
            current_icon.clear();
            draw_not_connected(&mut display);
            draw_volume(&mut display, standalone_value);
            needs_flush = true;
        }

        // Trigger a periodic flush every ~600 ms so the stats line stays live.
        if debug_visible && loop_tick % 200 == 0 {
            needs_flush = true;
        }

        if needs_flush {
            if debug_visible {
                // Redraw stats immediately before flush so `t=` is current.
                draw_rx_stats(&mut display, usb.stats(), loop_tick);
            }
            display.flush().expect("Flush failed");
            needs_flush = false;
        }

        backlight.tick(loop_tick);

        delay.delay_millis(1);
    }
}

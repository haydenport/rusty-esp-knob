# Rusty ESP Knob — Agent Orientation

This file is for future agents (and humans) working on the repo. It captures the *why*, the layout, and the non-obvious gotchas that aren't visible from skimming the source.

## What this project is

A physical per-app volume controller for Windows. The hardware is a Waveshare ESP32-S3 Knob Touch LCD 1.8" (round 360x360 AMOLED with a rotary encoder, capacitive touch, and a haptic motor). The user rotates the knob to change the volume of one selected Windows audio session, swipes the touchscreen to switch between apps, and taps to mute. A Windows companion app handles WASAPI and feeds app icons to the device.

## Layout

Cargo workspace, three crates:

- [firmware/](firmware/) — `no_std` Rust on `esp-hal` 1.0, targets `xtensa-esp32s3-none-elf`.
  - [src/main.rs](firmware/src/main.rs) — boot, peripheral init, main event loop.
  - [src/board.rs](firmware/src/board.rs) — **all pin assignments live here**. Touch this file when wiring changes.
  - [src/sh8601/](firmware/src/sh8601/) — QSPI driver + vendor init for the SH8601 AMOLED. Framebuffer is in internal SRAM (heap), not PSRAM — DMA from PSRAM was unstable and the workaround is unresolved. `mod.rs` also contains three scanline arc-fill methods (`fill_ring`, `fill_ring_270`, `fill_ring_arc`) used by the volume bar — see *Arc renderer* quirk below.
  - [src/encoder.rs](firmware/src/encoder.rs) — interrupt-driven rotary encoder with debounce.
  - [src/touch.rs](firmware/src/touch.rs) — CST816 touch over I2C.
  - [src/haptic.rs](firmware/src/haptic.rs) — DRV2605 haptic driver over I2C.
  - [src/backlight.rs](firmware/src/backlight.rs) — LEDC PWM backlight + idle auto-dim/off state machine.
  - [src/usb_serial.rs](firmware/src/usb_serial.rs) — USB-Serial/JTAG transport.

- [companion/](companion/) — Windows-only Rust binary (tray app + CLI subcommands).
  - [src/main.rs](companion/src/main.rs) — entry point, clap subcommands (`list`, `serial-test`, default = tray).
  - [src/audio.rs](companion/src/audio.rs) — WASAPI session enumeration, volume/mute.
  - [src/icons.rs](companion/src/icons.rs) — extracts app icons from running processes, converts to RGB565 for the display.
  - [src/worker.rs](companion/src/worker.rs) — background thread that owns the serial link and pumps protocol messages.
  - [src/tray_app.rs](companion/src/tray_app.rs) — system tray UI / settings menu.
  - [src/config.rs](companion/src/config.rs) — TOML settings persisted under the user's config dir.

- [protocol/](protocol/) — `no_std`-compatible shared crate.
  - [src/messages.rs](protocol/src/messages.rs) — postcard-serialized message enums used by both sides.
  - [src/codec.rs](protocol/src/codec.rs) — framing: postcard payload + CRC-8 + COBS, terminated by `0x00`.

## Build & flash

The ESP Rust toolchain (Xtensa) must be sourced before any firmware build:

```
. $HOME\export-esp.ps1
cd C:\repo\rusty-esp-knob\firmware
cargo build      # compile only — use to check for errors
cargo run        # flashes the device via the runner in .cargo/config.toml, then exits (no monitor)
```

- Don't run `espflash` or `cargo espflash` directly — the runner is already wired up via `.cargo/config.toml`.
- `cargo run` does not attach a serial monitor, so `esp-println` output is not visible during a normal flash. The USB-Serial/JTAG peripheral is shared between the flasher and the protocol transport — `esp-println` logging is **disabled in firmware** to keep the framing clean. If you need ad-hoc logs, route them elsewhere or temporarily re-enable `esp-println` and accept that the protocol link will break while it's on.

The companion is a standard `cargo run` from [companion/](companion/) — no special toolchain.

## Hardware notes

Schematic sheets (Waveshare ESP32-S3 Knob Touch LCD 1.8") are committed at [hardware/schematic/](hardware/schematic/) — five PNGs covering LCD & power, ESP32-S3-R8 module, ESP32 chip, other peripherals, and DAC.



Waveshare ESP32-S3 Knob Touch LCD 1.8":

- Display: SH8601 AMOLED (not ST77916, which earlier docs suggested), QSPI. Opcode `0x02` for register writes, `0x32` for pixel data (quad-wire).
- Touch: CST816 on I2C bus (SDA=GPIO11, SCL=GPIO12, INT=GPIO9, RST=GPIO10).
- Haptic: DRV2605 on the **same** I2C bus, address `0x5A`.
- Encoder: GPIO8 (A), GPIO7 (B), pull-up, interrupt-driven with debounce.
- Backlight: GPIO47.

## Protocol & transport

Wire format: `COBS(postcard(message) || crc8) || 0x00`. The transport is the ESP32-S3's built-in USB-Serial/JTAG — same physical interface used to flash the device. Both directions share the link; the framing terminator is what lets either side resync.

## Known quirks & history

- **Framebuffer in SRAM, not PSRAM.** DMA from PSRAM hit issues that weren't resolved; the 360x360x2 framebuffer fits in the 270KB internal heap.
- **Boot Ready handshake.** Earlier builds had a ~5s startup window where the companion would send before the device was ready, producing COBS errors. Fixed via a Ready message at firmware boot — the companion waits for it before sending app data.
- **Windows-only companion.** WASAPI + `tray-icon` are Windows. Cross-platform support would require swapping `audio.rs` and the tray backend.
- **Backlight wake gating.** A touch that wakes the screen from the Off state is *swallowed* by [main.rs](firmware/src/main.rs) (via `Backlight::wake_from_off`) so a wake-tap doesn't also toggle mute. Encoder turns always wake AND act, since rotating a sleeping knob is unambiguously volume intent. Heartbeat `Ping` from the companion intentionally does NOT count as activity — otherwise the screen would never dim.
- **Arc renderer — scanline, not embedded-graphics Arc.** The volume bar's `draw_volume` originally used three `embedded_graphics::Arc` calls. Each iterates its full 340×340 bounding box (~115 K pixels) with per-pixel trig, costing ~22 ms total — enough to make the knob feel laggy. Replaced with direct framebuffer scanline methods on `Sh8601`: `fill_ring` (360° wipe, distance-only check), `fill_ring_270` (270° gray track, integer `dy > |dx|` exclusion), and `fill_ring_arc` (partial white fill, cross-product sweep test — precomputes endpoint direction once, does 2 float muls per pixel, no `atan2`). Cap circles are still rendered via embedded-graphics `Circle` (tiny bounding box, negligible). Do not re-introduce `Arc` primitives for the volume bar.
- **Touch read is IRQ-gated.** `touch.read()` is only called when `irq_asserted() || is_finger_down()`, skipping the ~0.4 ms I2C transaction on idle ticks. The `is_finger_down()` arm is necessary: tap/long-press gestures are reported at finger-lift when the IRQ pin has already gone high, so gating on IRQ alone would miss them.
- **Knob-turn haptic is disabled.** The DRV2605 `SharpClick` on encoder steps was removed because the ~15 ms draw-then-haptic sequence felt out of phase with the detents. Haptic still fires on tap-to-mute and carousel swipes.
- **Project status.** Core feature set (encoder → volume, carousel, mute, tray, settings, Ready handshake, PWM backlight) is shipped. Future ideas: peak/VU meter on display, per-app volume profiles.

# Rusty ESP Knob

A per-app volume controller built on the [Waveshare ESP32-S3 Knob Touch LCD 1.8"](https://www.waveshare.com/wiki/ESP32-S3-Knob-Touch-LCD-1.8). Rotate the knob to change the volume of the currently selected Windows audio session, swipe the round 360x360 AMOLED to switch between apps, and tap to mute.

The firmware is `no_std` Rust on `esp-hal` 1.0; the PC companion is a Windows tray app that talks to the device over USB CDC and drives WASAPI.

## Repository layout

This is a Cargo workspace with three crates:

- [firmware/](firmware/) — `no_std` ESP32-S3 firmware. Display driver (SH8601 QSPI AMOLED), CST816 touch, DRV2605 haptics, rotary encoder, USB-Serial/JTAG transport, embedded-graphics UI.
- [companion/](companion/) — Windows tray app. Enumerates audio sessions via WASAPI, extracts app icons, ships them to the device, applies volume/mute changes from knob events.
- [protocol/](protocol/) — Shared message types and framing (postcard + CRC-8 + COBS) used by both sides.

See [CLAUDE.md](CLAUDE.md) for project orientation and build notes.

## Hardware

Waveshare ESP32-S3 Knob Touch LCD 1.8":

- 360x360 round AMOLED (SH8601 controller, QSPI)
- CST816 capacitive touch
- DRV2605 haptic motor driver
- Rotary encoder with detents (GPIO8/GPIO7)
- USB-C (USB-Serial/JTAG, used for both flashing and the host link)

Pin assignments live in [firmware/src/board.rs](firmware/src/board.rs).

## Building & flashing the firmware

The ESP Rust toolchain (Xtensa target) must be installed and sourced. On Windows PowerShell:

```powershell
. $HOME\export-esp.ps1
cd firmware
cargo build         # compile only
cargo run           # flash via the runner configured in .cargo/config.toml
```

Don't invoke `espflash` or `cargo espflash` directly — `cargo run` is wired to the correct runner.

## Running the companion

```powershell
cd companion
cargo run -- list                  # list current Windows audio sessions
cargo run -- serial-test           # protocol round-trip against the device
cargo run                          # launch the tray app (default)
```

The tray app auto-detects the device's serial port, pushes the current app list and icons, and applies volume/mute changes driven by encoder rotation and taps. Settings (default app, sensitivity, haptic strength, run-at-login) are reachable from the tray menu.

## Protocol

Frames are postcard-serialized messages with a CRC-8 trailer, COBS-encoded, terminated by `0x00`. The transport is the ESP32-S3's built-in USB-Serial/JTAG peripheral — the same interface used by the flasher — so `esp-println` logging is disabled in firmware to keep the byte stream clean. Message definitions are in [protocol/src/messages.rs](protocol/src/messages.rs); the codec is in [protocol/src/codec.rs](protocol/src/codec.rs).

## Platform support

The firmware is portable Rust on `esp-hal`, but the companion currently targets **Windows only** (WASAPI + tray-icon). Linux/macOS support would require a different audio backend.

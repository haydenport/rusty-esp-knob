# Volume Knob Controller - Implementation Plan

10-phase plan for building a per-app volume control knob using the
Waveshare ESP32-S3 1.8" Knob Touch LCD.

## Phase 1: Bare Metal Hello World (firmware only) - DONE

**Goal**: Verify toolchain, flash the board, get serial output.

Files:
- `Cargo.toml` (workspace)
- `firmware/Cargo.toml`, `firmware/.cargo/config.toml`, `firmware/rust-toolchain.toml`
- `firmware/src/main.rs` -- Initialize esp-hal, blink backlight (GPIO47), print to USB serial JTAG.
- `firmware/src/board.rs` -- All pin constants.

**Testable outcome**: Backlight toggles. `espflash monitor` shows debug prints.

## Phase 2: Display Driver (firmware only) - DONE

**Goal**: Render pixels on the 360x360 AMOLED display.

Files:
- `firmware/src/sh8601/mod.rs` -- QSPI driver, framebuffer, `DrawTarget` impl.
- `firmware/src/sh8601/init.rs` -- SH8601 vendor init sequence (ported from Waveshare Arduino demo).

**Testable outcome**: Solid color fill, test pattern (rectangle + circle) rendered on screen.

Note: Display uses SH8601 AMOLED controller (not ST77916 as originally assumed).
QSPI protocol: opcode 0x02 for register writes, 0x32 for pixel data (quad-wire).
Framebuffer lives in internal SRAM (270KB heap) -- PSRAM DMA issues unresolved.

## Phase 3: Touch + Encoder + Haptic Input (firmware only) - DONE

**Goal**: Read all hardware inputs and display feedback.

Files:
- `firmware/src/encoder.rs` -- Software-debounced rotary encoder (DONE)
- `firmware/src/touch/mod.rs`, `firmware/src/touch/cst816.rs` -- CST816 touch (I2C)
- `firmware/src/haptic.rs` -- DRV2605 haptic motor driver (I2C)

Hardware:
- Rotary encoder: GPIO8 (A), GPIO7 (B), pull-up, software debounce at 3ms polling
- Touch: CST816 on I2C (SDA=GPIO11, SCL=GPIO12, INT=GPIO9, RST=GPIO10)
- Haptic motor: DRV2605 on I2C (SDA=GPIO11, SCL=GPIO12, addr=0x5A)

**Testable outcome**: Rotating the encoder changes a number displayed on screen.
Swiping left/right changes a displayed page index. Tapping displays "TAP" text.
Haptic feedback fires on encoder detent or tap.

## Phase 4: Protocol Library + USB CDC (firmware + protocol crate) - DONE

**Goal**: Bidirectional structured communication over USB.

Files:
- `protocol/Cargo.toml`, `protocol/src/lib.rs`, `protocol/src/messages.rs`, `protocol/src/codec.rs`
- `firmware/src/usb_serial.rs`
- `companion/src/main.rs` -- Phase 4 test harness (Ping/Echo + listen for events).

Framing: postcard serialization + CRC-8 + COBS, 0x00 frame terminator.
Transport: USB-Serial/JTAG (shared with flasher). `esp-println` logger is
disabled to keep the byte stream clean — route logs elsewhere if needed later.

**Testable outcome**: Send a command from PC, firmware echoes it back.

## Phase 5: PC Companion - Audio Sessions (companion only) - DONE

**Goal**: Enumerate running audio sessions via WASAPI.

- List apps with active audio, get/set per-app volume.
- Windows-only, uses WASAPI COM APIs.

Files:
- `companion/src/audio.rs` -- WASAPI enumeration, `set_volume`, `set_mute`.
- `companion/src/main.rs` -- `companion list` subcommand (+ Phase 4 `serial-test`).

**Testable outcome**: CLI prints list of apps and their current volumes.

## Phase 6: PC Companion - Serial + Icon Extraction - DONE

**Goal**: Connect companion to device over USB CDC, extract app icons.

- Serial link using protocol crate from Phase 4.
- Extract app icons from running processes, convert to RGB565 for display.

**Testable outcome**: Companion sends app list + icons to firmware, firmware displays them.

## Phase 7: End-to-End Volume Control (integration)

**Goal**: Turn knob -> volume changes on PC for the selected app.

- Wire encoder events through protocol to companion.
- Companion adjusts WASAPI volume for targeted app.

**Testable outcome**: Full loop -- rotate knob, see volume change on both display and PC mixer.

## Phase 8: Carousel UI + Mute

**Goal**: Swipe through apps on display, tap to mute/unmute.

- Touch gestures drive app selection carousel.
- Tap toggles mute state via companion.
- Haptic feedback on mute toggle.

**Testable outcome**: Swipe between apps, tap to mute, visual + haptic confirmation.

## Phase 9: System Tray + Settings + Polish

**Goal**: Tray app, user settings, startup behavior.

- System tray icon with context menu.
- Auto-start on Windows login.
- Settings: default app, sensitivity, haptic strength.

**Testable outcome**: Companion runs in background, configurable via tray menu.

## Phase 10 (Future): Peak Meter + Profiles

**Goal**: Real-time audio visualization, per-app volume profiles.

- VU / peak meter rendered on display per active app.
- Save/restore volume profiles (e.g., "gaming", "music", "meeting").

**Testable outcome**: Live audio level animation, profile switching via touch.
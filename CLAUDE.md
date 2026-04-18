# Rusty ESP Knob

ESP32-S3 firmware for the Waveshare 1.8" Knob Touch LCD.

## Build & Flash

The ESP toolchain must be sourced before building:

```
. $HOME\export-esp.ps1
cd C:\repo\rusty-esp-knob\firmware
cargo build
cargo run
```

- `cargo build` compiles the firmware — use this to check for errors
- `cargo run` flashes to device and exits (no serial monitor) — use this to deploy
- Do NOT use `cargo espflash` or `espflash` directly — `cargo run` uses the runner configured in `.cargo/config.toml`

## Architecture

- `firmware/` — no_std Rust firmware (esp-hal 1.0, embedded-graphics)
- `protocol/` — shared message types and COBS codec (placeholder)
- `companion/` — Windows companion app (placeholder)

## Hardware

Waveshare ESP32-S3 Knob Touch LCD 1.8" — pin assignments in `firmware/src/board.rs`.

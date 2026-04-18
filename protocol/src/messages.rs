use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// Protocol version. Bump on breaking changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Messages sent from the PC companion to the firmware.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostToDevice {
    /// Liveness check — device replies with `Pong`.
    Ping,
    /// Provide the full list of currently-active audio apps.
    SetAppList(Vec<AppInfo>),
    /// Push an icon (RGB565) for a specific app.
    SetAppIcon { app_id: u32, pixels: Vec<u8> },
    /// Tell the device which app is currently selected.
    SetSelectedApp(u32),
    /// Set absolute volume for an app (0..=100).
    SetVolume { app_id: u32, level: u8 },
    /// Set mute state for an app.
    SetMute { app_id: u32, muted: bool },
    /// Echo test — device replies with `Echo` containing the same payload.
    Echo(Vec<u8>),
}

/// Messages sent from the firmware to the PC companion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DeviceToHost {
    /// Sent once at boot with the firmware's protocol version.
    Ready { version: u16 },
    /// Reply to `Ping`.
    Pong,
    /// Reply to `Echo` — the same payload received.
    Echo(Vec<u8>),
    /// Relative encoder movement (CW = positive).
    EncoderDelta(i32),
    /// Touch gesture fired by the user.
    Gesture(GestureKind),
    /// Request the host to adjust the selected app's volume by a relative amount.
    VolumeDelta { app_id: u32, delta: i8 },
    /// Notify the host that the user swiped to a different app.
    AppSelected(u32),
    /// Request the host to toggle mute on the given app.
    MuteToggle { app_id: u32 },
    /// Ack that the firmware has fully processed a command from the host.
    /// Used by the companion as flow control for large writes (icon pushes).
    Ack,
}

/// Minimal info about an audio app. Icons are sent separately via `SetAppIcon`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
    pub id: u32,
    pub name: String,
    pub volume: u8,
    pub muted: bool,
}

/// Touch gestures — mirrors the firmware `touch::Gesture` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GestureKind {
    SingleTap,
    LongPress,
    SwipeUp,
    SwipeDown,
    SwipeLeft,
    SwipeRight,
}

//! WASAPI audio session enumeration (Windows-only).
//!
//! Uses the default render endpoint's [`IAudioSessionManager2`] to list every
//! active per-application audio session, then [`ISimpleAudioVolume`] to read
//! and write volume/mute.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::core::PWSTR;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// A single per-application audio session.
#[derive(Debug, Clone)]
pub struct AudioSession {
    pub pid: u32,
    /// Executable name (e.g. "chrome.exe"). Empty for system sounds (pid 0)
    /// or processes we can't open.
    pub process_name: String,
    /// Current master volume, 0.0..=1.0.
    pub volume: f32,
    pub muted: bool,
}

/// Initialize COM for the current thread. Call once before any other
/// audio function on this thread. Safe to call multiple times.
pub fn init() -> windows::core::Result<()> {
    unsafe {
        // S_FALSE is returned if COM was already initialized on this thread —
        // not an error for our purposes.
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            return Err(hr.into());
        }
    }
    Ok(())
}

/// Enumerate every audio session on the default render endpoint.
pub fn enumerate() -> windows::core::Result<Vec<AudioSession>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        let count = sessions.GetCount()?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let ctrl = sessions.GetSession(i)?;
            let ctrl2: IAudioSessionControl2 = ctrl.cast()?;
            let vol: ISimpleAudioVolume = ctrl.cast()?;

            let pid = ctrl2.GetProcessId().unwrap_or(0);
            let volume = vol.GetMasterVolume().unwrap_or(0.0);
            let muted = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            let process_name = if pid != 0 {
                get_process_name(pid).unwrap_or_default()
            } else {
                String::from("System Sounds")
            };

            out.push(AudioSession {
                pid,
                process_name,
                volume,
                muted,
            });
        }

        Ok(out)
    }
}

/// Set the master volume (0.0..=1.0) for the first session matching `pid`.
pub fn set_volume(pid: u32, level: f32) -> windows::core::Result<()> {
    with_session(pid, |vol| unsafe {
        vol.SetMasterVolume(level.clamp(0.0, 1.0), std::ptr::null())
    })
}

/// Set the mute state for the first session matching `pid`.
pub fn set_mute(pid: u32, muted: bool) -> windows::core::Result<()> {
    with_session(pid, |vol| unsafe { vol.SetMute(muted, std::ptr::null()) })
}

/// Find the first session matching `pid` and run `f` on its `ISimpleAudioVolume`.
fn with_session<F>(pid: u32, f: F) -> windows::core::Result<()>
where
    F: FnOnce(&ISimpleAudioVolume) -> windows::core::Result<()>,
{
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        let count = sessions.GetCount()?;

        for i in 0..count {
            let ctrl = sessions.GetSession(i)?;
            let ctrl2: IAudioSessionControl2 = ctrl.cast()?;
            if ctrl2.GetProcessId().unwrap_or(0) == pid {
                let vol: ISimpleAudioVolume = ctrl.cast()?;
                return f(&vol);
            }
        }

        Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "no audio session matches pid",
        ))
    }
}

fn get_process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if result.is_err() || len == 0 {
            return None;
        }
        let full = OsString::from_wide(&buf[..len as usize])
            .to_string_lossy()
            .into_owned();
        // Strip directory, keep only the file name (e.g. "chrome.exe")
        Some(
            full.rsplit(['/', '\\'])
                .next()
                .unwrap_or(&full)
                .to_string(),
        )
    }
}

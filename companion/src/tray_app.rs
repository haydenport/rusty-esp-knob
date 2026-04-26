use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::config::{self, Config};
use crate::worker::{self, BacklightShared, WorkerStatus};

/// Entry point for tray mode. Blocks until the user chooses Exit.
pub fn run(port_name: String, cfg: Config) {
    // Build tray icon + menu before spawning the worker so the tray is
    // visible immediately even while audio enumeration runs.
    let icon = make_icon();
    let (menu, ids) = build_menu(&cfg);

    let _tray: TrayIcon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_tooltip("Rusty ESP Knob")
        .build()
        .expect("failed to build tray icon");

    // Shared state between the tray event loop and the serial worker.
    let stop = Arc::new(AtomicBool::new(false));
    let sensitivity = Arc::new(AtomicU8::new(cfg.sensitivity_pct));
    let backlight = Arc::new(BacklightShared::new(
        cfg.backlight_pct,
        cfg.backlight_dim_after_secs,
        cfg.backlight_off_after_secs,
    ));
    let (status_tx, status_rx) = mpsc::channel::<WorkerStatus>();

    let stop_worker = stop.clone();
    let sens_worker = sensitivity.clone();
    let backlight_worker = backlight.clone();
    std::thread::spawn(move || {
        worker::run(&port_name, sens_worker, backlight_worker, stop_worker, status_tx);
    });

    // Mutable local config copy for updating from menu events.
    let mut live_cfg = cfg;

    // Windows message pump + event polling loop.
    // tray-icon requires the Win32 message loop on the main thread to deliver
    // shell notification messages; we poll at ~20 Hz which is plenty for a tray.
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };

        loop {
            // Drain all pending Win32 messages.
            let mut msg = std::mem::zeroed::<MSG>();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Update tooltip from worker status.
            if let Ok(status) = status_rx.try_recv() {
                let tip = match &status {
                    WorkerStatus::Connected(p) => format!("Rusty ESP Knob — {p}"),
                    WorkerStatus::Disconnected => "Rusty ESP Knob — disconnected".into(),
                    WorkerStatus::Error(e) => format!("Rusty ESP Knob — {e}"),
                };
                let _ = _tray.set_tooltip(Some(tip));
            }

            // Handle menu item clicks.
            if let Ok(event) = MenuEvent::receiver().try_recv() {
                let id = &event.id;

                if id == &ids.exit {
                    stop.store(true, Ordering::Relaxed);
                    break;
                } else if id == &ids.autostart {
                    live_cfg.autostart = !live_cfg.autostart;
                    ids.autostart_item.set_checked(live_cfg.autostart);
                    set_autostart(live_cfg.autostart);
                    config::save(&live_cfg);
                } else if id == &ids.sens_1 {
                    set_sensitivity(&mut live_cfg, &sensitivity, &ids, 1);
                } else if id == &ids.sens_2 {
                    set_sensitivity(&mut live_cfg, &sensitivity, &ids, 2);
                } else if id == &ids.sens_5 {
                    set_sensitivity(&mut live_cfg, &sensitivity, &ids, 5);
                } else if let Some(pct) = ids.brightness_choice(id) {
                    set_brightness(&mut live_cfg, &backlight, &ids, pct);
                } else if let Some(secs) = ids.dim_choice(id) {
                    set_dim_after(&mut live_cfg, &backlight, &ids, secs);
                } else if let Some(secs) = ids.off_choice(id) {
                    set_off_after(&mut live_cfg, &backlight, &ids, secs);
                }
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn set_sensitivity(
    cfg: &mut Config,
    sensitivity: &Arc<AtomicU8>,
    ids: &MenuIds,
    pct: u8,
) {
    cfg.sensitivity_pct = pct;
    sensitivity.store(pct, Ordering::Relaxed);
    ids.sens_1_item.set_checked(pct == 1);
    ids.sens_2_item.set_checked(pct == 2);
    ids.sens_5_item.set_checked(pct == 5);
    config::save(cfg);
}

fn set_brightness(
    cfg: &mut Config,
    backlight: &Arc<BacklightShared>,
    ids: &MenuIds,
    pct: u8,
) {
    cfg.backlight_pct = pct;
    backlight.pct.store(pct, Ordering::Relaxed);
    backlight.dirty.store(true, Ordering::Release);
    for (item, val) in &ids.brightness_items {
        item.set_checked(*val == pct);
    }
    config::save(cfg);
}

fn set_dim_after(
    cfg: &mut Config,
    backlight: &Arc<BacklightShared>,
    ids: &MenuIds,
    secs: u16,
) {
    cfg.backlight_dim_after_secs = secs;
    backlight.dim_after_secs.store(secs, Ordering::Relaxed);
    backlight.dirty.store(true, Ordering::Release);
    for (item, val) in &ids.dim_items {
        item.set_checked(*val == secs);
    }
    config::save(cfg);
}

fn set_off_after(
    cfg: &mut Config,
    backlight: &Arc<BacklightShared>,
    ids: &MenuIds,
    secs: u16,
) {
    cfg.backlight_off_after_secs = secs;
    backlight.off_after_secs.store(secs, Ordering::Relaxed);
    backlight.dirty.store(true, Ordering::Release);
    for (item, val) in &ids.off_items {
        item.set_checked(*val == secs);
    }
    config::save(cfg);
}

// ── Menu ─────────────────────────────────────────────────────────────────────

struct MenuIds {
    exit: tray_icon::menu::MenuId,
    autostart: tray_icon::menu::MenuId,
    autostart_item: CheckMenuItem,
    sens_1: tray_icon::menu::MenuId,
    sens_2: tray_icon::menu::MenuId,
    sens_5: tray_icon::menu::MenuId,
    sens_1_item: CheckMenuItem,
    sens_2_item: CheckMenuItem,
    sens_5_item: CheckMenuItem,
    /// `(item, brightness_pct)` pairs — first match wins.
    brightness_items: Vec<(CheckMenuItem, u8)>,
    /// `(item, dim_after_secs)` pairs. `0` = "Never".
    dim_items: Vec<(CheckMenuItem, u16)>,
    /// `(item, off_after_secs)` pairs. `0` = "Never".
    off_items: Vec<(CheckMenuItem, u16)>,
}

impl MenuIds {
    fn brightness_choice(&self, id: &tray_icon::menu::MenuId) -> Option<u8> {
        self.brightness_items
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|(_, v)| *v)
    }
    fn dim_choice(&self, id: &tray_icon::menu::MenuId) -> Option<u16> {
        self.dim_items
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|(_, v)| *v)
    }
    fn off_choice(&self, id: &tray_icon::menu::MenuId) -> Option<u16> {
        self.off_items
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|(_, v)| *v)
    }
}

fn build_menu(cfg: &Config) -> (Menu, MenuIds) {
    let menu = Menu::new();

    let title = MenuItem::new("Rusty ESP Knob", false, None);
    let _ = menu.append(&title);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let pct = cfg.sensitivity_pct;
    let sens_1 = CheckMenuItem::new("1%", true, pct == 1, None);
    let sens_2 = CheckMenuItem::new("2%", true, pct == 2, None);
    let sens_5 = CheckMenuItem::new("5%", true, pct == 5, None);
    let sens_sub = Submenu::with_items("Sensitivity", true, &[&sens_1, &sens_2, &sens_5])
        .expect("submenu");
    let _ = menu.append(&sens_sub);

    // Backlight brightness submenu.
    let brightness_choices: [u8; 4] = [25, 50, 75, 100];
    let brightness_items: Vec<(CheckMenuItem, u8)> = brightness_choices
        .iter()
        .map(|&p| {
            let label = format!("{p}%");
            (CheckMenuItem::new(&label, true, cfg.backlight_pct == p, None), p)
        })
        .collect();
    let brightness_refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
        brightness_items.iter().map(|(i, _)| i as &dyn tray_icon::menu::IsMenuItem).collect();
    let brightness_sub = Submenu::with_items("Brightness", true, &brightness_refs)
        .expect("brightness submenu");
    let _ = menu.append(&brightness_sub);

    // Dim-after submenu.
    let dim_choices: [(u16, &str); 5] = [
        (10, "10 s"),
        (30, "30 s"),
        (60, "1 min"),
        (180, "3 min"),
        (0, "Never"),
    ];
    let dim_items: Vec<(CheckMenuItem, u16)> = dim_choices
        .iter()
        .map(|(s, lbl)| {
            (CheckMenuItem::new(*lbl, true, cfg.backlight_dim_after_secs == *s, None), *s)
        })
        .collect();
    let dim_refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
        dim_items.iter().map(|(i, _)| i as &dyn tray_icon::menu::IsMenuItem).collect();
    let dim_sub = Submenu::with_items("Dim after", true, &dim_refs)
        .expect("dim submenu");
    let _ = menu.append(&dim_sub);

    // Off-after submenu (counted from the dim transition, not last activity).
    let off_choices: [(u16, &str); 5] = [
        (30, "30 s"),
        (90, "90 s"),
        (300, "5 min"),
        (600, "10 min"),
        (0, "Never"),
    ];
    let off_items: Vec<(CheckMenuItem, u16)> = off_choices
        .iter()
        .map(|(s, lbl)| {
            (CheckMenuItem::new(*lbl, true, cfg.backlight_off_after_secs == *s, None), *s)
        })
        .collect();
    let off_refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
        off_items.iter().map(|(i, _)| i as &dyn tray_icon::menu::IsMenuItem).collect();
    let off_sub = Submenu::with_items("Off after dim", true, &off_refs)
        .expect("off submenu");
    let _ = menu.append(&off_sub);

    let autostart = CheckMenuItem::new("Auto-start on login", true, cfg.autostart, None);
    let _ = menu.append(&autostart);

    let _ = menu.append(&PredefinedMenuItem::separator());

    let exit = MenuItem::new("Exit", true, None);
    let _ = menu.append(&exit);

    let ids = MenuIds {
        exit: exit.id().clone(),
        autostart: autostart.id().clone(),
        autostart_item: autostart,
        sens_1: sens_1.id().clone(),
        sens_2: sens_2.id().clone(),
        sens_5: sens_5.id().clone(),
        sens_1_item: sens_1,
        sens_2_item: sens_2,
        sens_5_item: sens_5,
        brightness_items,
        dim_items,
        off_items,
    };

    (menu, ids)
}

// ── Icon ─────────────────────────────────────────────────────────────────────

/// Generate a 32×32 RGBA tray icon: rust-coloured ring with a white
/// indicator line at 12 o'clock — a recognisable volume-knob silhouette.
fn make_icon() -> Icon {
    const W: usize = 32;
    const H: usize = 32;
    const CX: f32 = 15.5;
    const CY: f32 = 15.5;
    const R_OUTER: f32 = 14.0;
    const R_RING: f32 = 11.5; // inner edge of the ring

    // Rust #B7410E, dark interior, white indicator
    let ring: [u8; 4] = [0xB7, 0x41, 0x0E, 0xFF];
    let interior: [u8; 4] = [0x40, 0x18, 0x08, 0xFF];
    let indicator: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

    let mut px = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let dx = x as f32 - CX;
            let dy = y as f32 - CY;
            let dist = (dx * dx + dy * dy).sqrt();
            let i = (y * W + x) * 4;

            if dist > R_OUTER {
                continue; // transparent
            }

            if dist >= R_RING {
                px[i..i + 4].copy_from_slice(&ring);
            } else {
                px[i..i + 4].copy_from_slice(&interior);
                // Indicator: 2-px wide line from centre toward 12 o'clock.
                if (x == 15 || x == 16) && dy < -2.0 && dist < R_RING - 1.0 {
                    px[i..i + 4].copy_from_slice(&indicator);
                }
            }
        }
    }

    Icon::from_rgba(px, W as u32, H as u32).expect("icon")
}

// ── Auto-start registry ───────────────────────────────────────────────────────

fn set_autostart(enable: bool) {
    use windows::Win32::System::Registry::*;
    use windows::core::w;

    unsafe {
        let mut key = HKEY::default();
        let sub = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let name = w!("RustyEspKnob");

        if RegOpenKeyExW(HKEY_CURRENT_USER, sub, 0, KEY_SET_VALUE, &mut key).is_err() {
            eprintln!("autostart: failed to open Run key");
            return;
        }

        if enable {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = format!("\"{}\" tray", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * 2,
            );
            if RegSetValueExW(key, name, 0, REG_SZ, Some(bytes)).is_err() {
                eprintln!("autostart: failed to set registry value");
            }
        } else {
            let _ = RegDeleteValueW(key, name);
        }

        let _ = RegCloseKey(key);
    }
}

mod audio;
mod config;
mod icons;
mod tray_app;
mod worker;

use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use protocol::codec::Decoder;
use protocol::messages::{AppInfo, HostToDevice};
use serialport::ClearBuffer;

use crate::worker::BacklightShared;

#[derive(Parser)]
#[command(name = "companion", about = "Volume Knob PC companion")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List every WASAPI audio session on the default render endpoint.
    List,
    /// Phase 4: open a serial port, send Ping/Echo, listen for events.
    SerialTest {
        port: String,
    },
    /// Push app list + icons then run the volume-control event loop (CLI mode).
    Run {
        /// COM port (e.g. "COM8"). If omitted, auto-detects by USB VID/PID.
        port: Option<String>,
    },
    /// Run as a Windows system-tray application (background mode).
    Tray {
        /// COM port override (e.g. "COM8"). Overrides config file and auto-detect.
        #[arg(short, long)]
        port: Option<String>,
    },
    /// Extract icons for every audio session and save as PPM files (debug).
    DumpIcons,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::List => cmd_list(),
        Command::SerialTest { port } => cmd_serial_test(&port),
        Command::Run { port } => cmd_run(port),
        Command::Tray { port } => cmd_tray(port),
        Command::DumpIcons => cmd_dump_icons(),
    }
}

// ── Tray ─────────────────────────────────────────────────────────────────────

fn cmd_tray(port_override: Option<String>) {
    // Detach from console so no terminal window lingers in the background.
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }

    if let Err(e) = audio::init() {
        // Can't show console, so silently exit; tray tooltip will show error.
        eprintln!("COM init failed: {e}");
        std::process::exit(1);
    }

    let mut cfg = config::load();

    // Resolve the COM port: CLI arg > config file > auto-detect.
    let port_name = port_override
        .filter(|s| !s.is_empty())
        .or_else(|| Some(cfg.port.clone()).filter(|s| !s.is_empty()))
        .or_else(config::find_device_port)
        .unwrap_or_else(|| {
            eprintln!("no device found; use --port or set port in config");
            std::process::exit(1);
        });

    // Keep port in config so next launch remembers it.
    if cfg.port != port_name {
        cfg.port = port_name.clone();
        config::save(&cfg);
    }

    tray_app::run(port_name, cfg);
}

// ── Run (CLI fallback) ────────────────────────────────────────────────────────

fn cmd_run(port_override: Option<String>) {
    if let Err(e) = audio::init() {
        eprintln!("COM init failed: {e}");
        std::process::exit(1);
    }

    let cfg = config::load();

    let port_name = port_override
        .filter(|s| !s.is_empty())
        .or_else(|| Some(cfg.port.clone()).filter(|s| !s.is_empty()))
        .or_else(config::find_device_port)
        .unwrap_or_else(|| {
            eprintln!("no device found; use `run <PORT>` or configure port");
            std::process::exit(1);
        });

    println!("using port {port_name}  (sensitivity {}%)", cfg.sensitivity_pct);

    let stop = Arc::new(AtomicBool::new(false));
    let sensitivity = Arc::new(AtomicU8::new(cfg.sensitivity_pct));
    let backlight = Arc::new(BacklightShared::new(
        cfg.backlight_pct,
        cfg.backlight_dim_after_secs,
        cfg.backlight_off_after_secs,
    ));
    let (status_tx, _status_rx) = std::sync::mpsc::channel();

    // Run the event loop on the current thread (blocking until port error or Ctrl+C).
    worker::run(&port_name, sensitivity, backlight, stop, status_tx);
}

// ── List ─────────────────────────────────────────────────────────────────────

fn cmd_list() {
    if let Err(e) = audio::init() {
        eprintln!("COM init failed: {e}");
        std::process::exit(1);
    }

    let sessions = match audio::enumerate() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("enumerate failed: {e}");
            std::process::exit(1);
        }
    };

    if sessions.is_empty() {
        println!("(no active audio sessions)");
        return;
    }

    println!("{:<8} {:<6} {:<6} {}", "PID", "VOL%", "MUTE", "PROCESS");
    for s in &sessions {
        println!(
            "{:<8} {:<6} {:<6} {}",
            s.pid,
            (s.volume * 100.0).round() as u32,
            if s.muted { "yes" } else { "no" },
            s.process_name,
        );
    }
}

// ── DumpIcons ─────────────────────────────────────────────────────────────────

fn cmd_dump_icons() {
    if let Err(e) = audio::init() {
        eprintln!("COM init failed: {e}");
        std::process::exit(1);
    }
    let sessions = match audio::enumerate() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("enumerate failed: {e}");
            std::process::exit(1);
        }
    };
    for s in sessions.iter().filter(|s| !s.exe_path.is_empty()) {
        let out = format!("icon-{}-{}.ppm", s.pid, s.process_name);
        match icons::dump_ppm(&s.exe_path, &out) {
            Ok(()) => println!("wrote {out}"),
            Err(e) => eprintln!("{}: {e}", s.process_name),
        }
    }
}

// ── SerialTest ────────────────────────────────────────────────────────────────

fn cmd_serial_test(port_name: &str) {
    let mut port = serialport::new(port_name, 115_200)
        .timeout(Duration::from_millis(50))
        .open()
        .unwrap_or_else(|e| {
            eprintln!("failed to open {port_name}: {e}");
            std::process::exit(1);
        });

    println!("opened {port_name}");
    std::thread::sleep(Duration::from_millis(200));

    let mut decoder = Decoder::new(16 * 1024);
    let _ = worker::drain_for(&mut *port, Duration::from_millis(100), &mut decoder);
    decoder = Decoder::new(16 * 1024);

    println!("\n>>> sending Ping");
    worker::send(&mut *port, &HostToDevice::Ping);
    worker::drain_for(&mut *port, Duration::from_millis(500), &mut decoder);

    let payload = b"hello, knob!".to_vec();
    println!("\n>>> sending Echo({:?})", String::from_utf8_lossy(&payload));
    worker::send(&mut *port, &HostToDevice::Echo(payload));
    worker::drain_for(&mut *port, Duration::from_millis(500), &mut decoder);

    println!("\n>>> listening for 10s — turn the knob or tap the screen");
    worker::drain_for(&mut *port, Duration::from_secs(10), &mut decoder);

    println!("\ndone.");
}

// ── PushApps (kept for debugging) ────────────────────────────────────────────

// The old push-apps command is superseded by `run`; kept as dead code for now.
#[allow(dead_code)]
fn cmd_push_apps(port_name: &str) {
    if let Err(e) = audio::init() {
        eprintln!("COM init failed: {e}");
        std::process::exit(1);
    }

    let sessions = match audio::enumerate() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("enumerate failed: {e}");
            std::process::exit(1);
        }
    };

    let mut app_infos: Vec<AppInfo> = Vec::new();
    let mut icons_out: Vec<(u32, Vec<u8>)> = Vec::new();

    for (idx, s) in sessions.iter().enumerate() {
        if s.exe_path.is_empty() {
            continue;
        }
        let id = s.pid;
        app_infos.push(AppInfo {
            id,
            name: s.process_name.clone(),
            volume: (s.volume * 100.0).round().clamp(0.0, 100.0) as u8,
            muted: s.muted,
        });
        match icons::extract_rgb565(&s.exe_path) {
            Some(px) => {
                println!("[{idx}] {} ({}B icon)", s.process_name, px.len());
                icons_out.push((id, px));
            }
            None => println!("[{idx}] {} (no icon)", s.process_name),
        }
    }

    if app_infos.is_empty() {
        println!("(no pushable audio sessions)");
        return;
    }

    let mut port = serialport::new(port_name, 115_200)
        .timeout(Duration::from_secs(5))
        .open()
        .unwrap_or_else(|e| {
            eprintln!("failed to open {port_name}: {e}");
            std::process::exit(1);
        });

    println!("opened {port_name}");
    std::thread::sleep(Duration::from_millis(200));

    let mut decoder = Decoder::new(16 * 1024);
    let _ = worker::drain_for(&mut *port, Duration::from_millis(100), &mut decoder);
    let _ = port.clear(ClearBuffer::Input);
    decoder = Decoder::new(16 * 1024);

    let first_id = app_infos[0].id;
    println!("\n>>> sending SetAppList ({} apps)", app_infos.len());
    worker::send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetAppList(app_infos));
    println!(">>> sending SetSelectedApp({first_id})");
    worker::send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetSelectedApp(first_id));
    for (app_id, pixels) in icons_out {
        println!(">>> sending SetAppIcon(app_id={app_id}, {}B)", pixels.len());
        worker::send_and_wait(
            &mut *port,
            &mut decoder,
            &HostToDevice::SetAppIcon { app_id, pixels },
        );
    }

    println!("\n>>> listening for 3s");
    worker::drain_for(&mut *port, Duration::from_secs(3), &mut decoder);
    println!("\ndone.");
}

mod audio;
mod icons;

use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use protocol::codec::{self, Decoder};
use protocol::messages::{AppInfo, DeviceToHost, HostToDevice};

#[derive(Parser)]
#[command(name = "companion", about = "Volume Knob PC companion")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Phase 5: list every WASAPI audio session on the default render endpoint.
    List,
    /// Phase 4: open a serial port, send Ping/Echo, listen for events.
    SerialTest {
        /// COM port the device appears as (e.g. "COM8").
        port: String,
    },
    /// Phase 6: enumerate audio sessions, extract icons, push to firmware.
    PushApps {
        /// COM port the device appears as (e.g. "COM8").
        port: String,
    },
    /// Phase 6 debug: extract icons for every audio session, save as PPM files.
    DumpIcons,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::List => cmd_list(),
        Command::SerialTest { port } => cmd_serial_test(&port),
        Command::PushApps { port } => cmd_push_apps(&port),
        Command::DumpIcons => cmd_dump_icons(),
    }
}

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
    let _ = drain_for(&mut *port, Duration::from_millis(100), &mut Decoder::new(16 * 1024));

    let mut decoder = Decoder::new(16 * 1024);

    println!("\n>>> sending Ping");
    send(&mut *port, &HostToDevice::Ping);
    drain_for(&mut *port, Duration::from_millis(500), &mut decoder);

    let payload = b"hello, knob!".to_vec();
    println!("\n>>> sending Echo({:?})", String::from_utf8_lossy(&payload));
    send(&mut *port, &HostToDevice::Echo(payload));
    drain_for(&mut *port, Duration::from_millis(500), &mut decoder);

    println!("\n>>> listening for 10s — turn the knob or tap the screen");
    drain_for(&mut *port, Duration::from_secs(10), &mut decoder);

    println!("\ndone.");
}

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

    // Build the AppInfo list + per-app icon bytes in one pass. Skip sessions
    // whose exe we can't see (pid 0, protected processes) — they have no icon.
    let mut app_infos: Vec<AppInfo> = Vec::new();
    let mut icons_out: Vec<(u32, Vec<u8>)> = Vec::new();

    for (idx, s) in sessions.iter().enumerate() {
        if s.exe_path.is_empty() {
            continue;
        }
        // App id = the WASAPI pid. Unique within a single enumeration pass,
        // which is all the firmware needs right now.
        let id = s.pid;
        let info = AppInfo {
            id,
            name: s.process_name.clone(),
            volume: (s.volume * 100.0).round().clamp(0.0, 100.0) as u8,
            muted: s.muted,
        };
        app_infos.push(info);

        match icons::extract_rgb565(&s.exe_path) {
            Some(px) => {
                println!(
                    "[{idx}] {} ({}B icon)",
                    s.process_name,
                    px.len()
                );
                icons_out.push((id, px));
            }
            None => {
                println!("[{idx}] {} (no icon)", s.process_name);
            }
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
    let _ = drain_for(&mut *port, Duration::from_millis(100), &mut decoder);

    let first_id = app_infos[0].id;

    println!("\n>>> sending SetAppList ({} apps)", app_infos.len());
    send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetAppList(app_infos));

    println!(">>> sending SetSelectedApp({first_id})");
    send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetSelectedApp(first_id));

    for (app_id, pixels) in icons_out {
        println!(">>> sending SetAppIcon(app_id={app_id}, {}B)", pixels.len());
        send_and_wait(
            &mut *port,
            &mut decoder,
            &HostToDevice::SetAppIcon { app_id, pixels },
        );
    }

    println!("\n>>> listening for 3s");
    drain_for(&mut *port, Duration::from_secs(3), &mut decoder);

    println!("\ndone.");
}

/// Send a message, then block until the firmware responds with `Ack` (or a
/// reasonable timeout elapses). Ack-based flow control: the firmware finishes
/// processing + drawing + flushing before acking, so by the time this returns
/// the device is ready for the next command.
fn send_and_wait(
    port: &mut dyn serialport::SerialPort,
    decoder: &mut Decoder,
    msg: &HostToDevice,
) {
    send(port, msg);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let _ = decoder.push(&buf[..n]);
                loop {
                    match decoder.next_frame::<DeviceToHost>() {
                        Ok(Some(DeviceToHost::Ack)) => {
                            return;
                        }
                        Ok(Some(other)) => println!("<<< {other:?}"),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("<<< decode error: {e:?}");
                            break;
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("read error: {e}");
                return;
            }
        }
    }
    eprintln!("!!! timed out waiting for Ack");
}

fn send(port: &mut dyn serialport::SerialPort, msg: &HostToDevice) {
    let bytes = match codec::encode(msg) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("encode error: {e:?}");
            return;
        }
    };

    if let Err(e) = port.write_all(&bytes) {
        eprintln!("write error: {e}");
    }
}

fn drain_for(
    port: &mut dyn serialport::SerialPort,
    duration: Duration,
    decoder: &mut Decoder,
) -> usize {
    let deadline = Instant::now() + duration;
    let mut buf = [0u8; 256];
    let mut total = 0;

    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                total += n;
                let _ = decoder.push(&buf[..n]);
                loop {
                    match decoder.next_frame::<DeviceToHost>() {
                        Ok(Some(msg)) => println!("<<< {msg:?}"),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("<<< decode error: {e:?}");
                            break;
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }
    }

    total
}

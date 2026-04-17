mod audio;

use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use protocol::codec::{self, Decoder};
use protocol::messages::{DeviceToHost, HostToDevice};

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
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::List => cmd_list(),
        Command::SerialTest { port } => cmd_serial_test(&port),
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

fn send(port: &mut dyn serialport::SerialPort, msg: &HostToDevice) {
    match codec::encode(msg) {
        Ok(bytes) => {
            if let Err(e) = port.write_all(&bytes) {
                eprintln!("write error: {e}");
            }
        }
        Err(e) => eprintln!("encode error: {e:?}"),
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

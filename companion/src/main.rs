//! Phase 4 test harness: send Ping/Echo to the firmware over USB-Serial-JTAG
//! and print everything the firmware sends back.
//!
//! Usage: `companion <COM_PORT>` (e.g. `companion COM7`)

use std::time::{Duration, Instant};

use protocol::codec::{self, Decoder};
use protocol::messages::{DeviceToHost, HostToDevice};

fn main() {
    let port_name = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: companion <COM_PORT>");
        eprintln!("available ports:");
        if let Ok(ports) = serialport::available_ports() {
            for p in ports {
                eprintln!("  {}", p.port_name);
            }
        }
        std::process::exit(1);
    });

    let mut port = serialport::new(&port_name, 115_200)
        .timeout(Duration::from_millis(50))
        .open()
        .unwrap_or_else(|e| {
            eprintln!("failed to open {port_name}: {e}");
            std::process::exit(1);
        });

    println!("opened {port_name}");

    // Give the firmware a moment to settle and drain whatever banner bytes are sitting there.
    std::thread::sleep(Duration::from_millis(200));
    let _ = drain_for(&mut *port, Duration::from_millis(100), &mut Decoder::new(16 * 1024));

    let mut decoder = Decoder::new(16 * 1024);

    // 1. Ping → expect Pong
    println!("\n>>> sending Ping");
    send(&mut *port, &HostToDevice::Ping);
    drain_for(&mut *port, Duration::from_millis(500), &mut decoder);

    // 2. Echo → expect Echo(same payload)
    let payload = b"hello, knob!".to_vec();
    println!("\n>>> sending Echo({:?})", String::from_utf8_lossy(&payload));
    send(&mut *port, &HostToDevice::Echo(payload));
    drain_for(&mut *port, Duration::from_millis(500), &mut decoder);

    // 3. Listen for encoder/gesture events for a few seconds.
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

/// Read bytes from the port for up to `duration`, feeding them through the
/// decoder. Prints decoded messages and any non-frame bytes as a log line.
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
                // Separate printable log bytes from frame bytes for display only.
                // The decoder always sees every byte so the 0x00 boundaries stay intact.
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

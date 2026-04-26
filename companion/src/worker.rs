use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::codec::{self, Decoder};
use protocol::messages::{AppInfo, DeviceToHost, HostToDevice};
use serialport::ClearBuffer;

use crate::audio;
use crate::icons;

pub enum WorkerStatus {
    Connected(String),
    Disconnected,
    Error(String),
}

/// Shared backlight settings. The tray writes via the setters; the worker
/// pushes a `SetBacklight` to the device whenever `dirty` is set.
pub struct BacklightShared {
    pub pct: AtomicU8,
    pub dim_after_secs: AtomicU16,
    pub off_after_secs: AtomicU16,
    pub dirty: AtomicBool,
}

impl BacklightShared {
    pub fn new(pct: u8, dim_after_secs: u16, off_after_secs: u16) -> Self {
        Self {
            pct: AtomicU8::new(pct),
            dim_after_secs: AtomicU16::new(dim_after_secs),
            off_after_secs: AtomicU16::new(off_after_secs),
            // Push once on connect.
            dirty: AtomicBool::new(true),
        }
    }

    pub fn snapshot(&self) -> HostToDevice {
        HostToDevice::SetBacklight {
            active_pct: self.pct.load(Ordering::Relaxed),
            dim_after_secs: self.dim_after_secs.load(Ordering::Relaxed),
            off_after_secs: self.off_after_secs.load(Ordering::Relaxed),
        }
    }
}

/// Open `port_name`, run the full init sequence (including Ready handshake),
/// then loop processing encoder/gesture events until `stop` is set.
///
/// `sensitivity` is a shared atomic so the tray can update it live.
/// `status_tx` receives coarse connection-state updates for the tray tooltip.
pub fn run(
    port_name: &str,
    sensitivity: Arc<AtomicU8>,
    backlight: Arc<BacklightShared>,
    stop: Arc<AtomicBool>,
    status_tx: std::sync::mpsc::Sender<WorkerStatus>,
) {
    // Initialize COM for this thread. In CLI mode the caller already did this
    // on the same thread (S_FALSE = already initialized, not an error). In tray
    // mode the worker runs on a spawned thread that hasn't initialized COM yet.
    if let Err(e) = audio::init() {
        eprintln!("COM init failed on worker thread: {e}");
        let _ = status_tx.send(WorkerStatus::Error(format!("COM: {e}")));
        return;
    }

    let sessions = match audio::enumerate() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("audio enumerate failed: {e}");
            let _ = status_tx.send(WorkerStatus::Error(format!("audio: {e}")));
            return;
        }
    };

    let mut app_infos: Vec<AppInfo> = Vec::new();
    let mut volumes: HashMap<u32, f32> = HashMap::new();
    let mut mutes: HashMap<u32, bool> = HashMap::new();
    let mut icons: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut seen_pids: HashSet<u32> = HashSet::new();

    for (idx, s) in sessions.iter().enumerate() {
        // Skip system-sounds (pid 0) and duplicate PIDs (multiple streams per app).
        if s.pid == 0 || !seen_pids.insert(s.pid) {
            continue;
        }
        let id = s.pid;
        app_infos.push(AppInfo {
            id,
            name: s.process_name.clone(),
            volume: (s.volume * 100.0).round().clamp(0.0, 100.0) as u8,
            muted: s.muted,
        });
        volumes.insert(id, s.volume);
        mutes.insert(id, s.muted);
        if !s.exe_path.is_empty() {
            match icons::extract_rgb565(&s.exe_path) {
                Some(px) => {
                    println!("[{idx}] {} pid={id} ({}B icon)", s.process_name, px.len());
                    icons.insert(id, px);
                }
                None => println!("[{idx}] {} pid={id} (no icon)", s.process_name),
            }
        } else {
            println!("[{idx}] {} pid={id} (no exe path)", s.process_name);
        }
    }

    if app_infos.is_empty() {
        println!("(no pushable audio sessions)");
        let _ = status_tx.send(WorkerStatus::Error("no audio sessions".into()));
        return;
    }

    let mut port = match serialport::new(port_name, 115_200)
        .timeout(Duration::from_secs(2))
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open {port_name}: {e}");
            let _ = status_tx.send(WorkerStatus::Error(format!("port: {e}")));
            return;
        }
    };

    println!("opened {port_name}");
    let _ = status_tx.send(WorkerStatus::Connected(port_name.to_string()));

    std::thread::sleep(Duration::from_millis(200));
    let mut decoder = Decoder::new(16 * 1024);
    let _ = drain_for(&mut *port, Duration::from_millis(100), &mut decoder);
    let _ = port.clear(ClearBuffer::Input);
    decoder = Decoder::new(16 * 1024);

    // Ready handshake: send Ping, wait for the firmware's Ready reply.
    // This guarantees USB CDC is fully up before we blast the init sequence,
    // eliminating the first-run COBS decode error from stale enumeration bytes.
    println!(">>> waiting for Ready (sending Ping)...");
    send(&mut *port, &HostToDevice::Ping);
    wait_for_ready(&mut *port, &mut decoder);

    let first_id = app_infos[0].id;
    println!(">>> sending SetAppList ({} apps)", app_infos.len());
    send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetAppList(app_infos.clone()));
    println!(">>> SetAppList ack'd");
    println!(">>> sending SetSelectedApp({first_id})");
    send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetSelectedApp(first_id));
    println!(">>> SetSelectedApp ack'd");
    for (&app_id, pixels) in &icons {
        println!(">>> sending SetAppIcon(app_id={app_id}, {}B)", pixels.len());
        send_and_wait(
            &mut *port,
            &mut decoder,
            &HostToDevice::SetAppIcon { app_id, pixels: pixels.clone() },
        );
        println!(">>> SetAppIcon({app_id}) ack'd");
    }

    println!(">>> running — Ctrl+C or tray Exit to stop");
    let mut buf = [0u8; 256];
    let mut last_refresh = Instant::now();
    let mut last_heartbeat = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let _ = decoder.push(&buf[..n]);
                loop {
                    match decoder.next_frame::<DeviceToHost>() {
                        Ok(Some(DeviceToHost::VolumeDelta { app_id, delta })) => {
                            let sens = sensitivity.load(Ordering::Relaxed) as f32 / 100.0;
                            let current = *volumes.get(&app_id).unwrap_or(&0.5);
                            let new_vol = (current + delta as f32 * sens).clamp(0.0, 1.0);
                            match audio::set_volume(app_id, new_vol) {
                                Ok(()) => {
                                    volumes.insert(app_id, new_vol);
                                    let level = (new_vol * 100.0).round() as u8;
                                    println!("vol {app_id} → {level}%");
                                    send(&mut *port, &HostToDevice::SetVolume { app_id, level });
                                }
                                Err(e) => eprintln!("set_volume({app_id}): {e}"),
                            }
                        }
                        Ok(Some(DeviceToHost::AppSelected(app_id))) => {
                            println!("<<< AppSelected({app_id})");
                            // Send SetVolume first and wait for Ack — the firmware
                            // flushes the display after SetVolume, and we must wait
                            // for that flush to complete before sending the large
                            // icon frame, otherwise the USB RX FIFO backs up and
                            // the write fails with ERROR_SEM_TIMEOUT.
                            if let Some(&vol) = volumes.get(&app_id) {
                                let level = (vol * 100.0).round() as u8;
                                println!("  → SetVolume({app_id}, {level}%)");
                                send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetVolume { app_id, level });
                            } else {
                                println!("  → no volume entry for pid {app_id}");
                            }
                            if let Some(pixels) = icons.get(&app_id) {
                                let pixels = pixels.clone();
                                println!("  → SetAppIcon({app_id}, {}B)", pixels.len());
                                send_and_wait(&mut *port, &mut decoder, &HostToDevice::SetAppIcon { app_id, pixels });
                            } else {
                                println!("  → no icon for pid {app_id}");
                            }
                        }
                        Ok(Some(DeviceToHost::MuteToggle { app_id })) => {
                            let currently = *mutes.get(&app_id).unwrap_or(&false);
                            let new_muted = !currently;
                            match audio::set_mute(app_id, new_muted) {
                                Ok(()) => {
                                    mutes.insert(app_id, new_muted);
                                    println!("mute {app_id} → {new_muted}");
                                    send(
                                        &mut *port,
                                        &HostToDevice::SetMute { app_id, muted: new_muted },
                                    );
                                }
                                Err(e) => eprintln!("set_mute({app_id}): {e}"),
                            }
                        }
                        Ok(Some(DeviceToHost::Ack)) => {}
                        Ok(Some(DeviceToHost::Pong)) => {}
                        Ok(Some(other)) => println!("<<< {other:?}"),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("<<< decode error: {e:?}");
                            let _ = port.clear(ClearBuffer::Input);
                            decoder.reset();
                            break;
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("read error: {e}");
                let _ = status_tx.send(WorkerStatus::Disconnected);
                break;
            }
        }

        // Heartbeat: keep the firmware's disconnect timer from firing while connected.
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            last_heartbeat = Instant::now();
            send(&mut *port, &HostToDevice::Ping);
        }

        // Push backlight settings if the tray changed them (or on first run
        // after connect, since `dirty` starts true).
        if backlight.dirty.swap(false, Ordering::AcqRel) {
            send(&mut *port, &backlight.snapshot());
        }

        // Periodically re-enumerate audio sessions so newly-launched apps appear.
        if last_refresh.elapsed() >= Duration::from_secs(5) {
            last_refresh = Instant::now();
            if let Ok(new_sessions) = audio::enumerate() {
                let mut new_app_infos: Vec<AppInfo> = Vec::new();
                let mut seen: HashSet<u32> = HashSet::new();
                for s in &new_sessions {
                    if s.pid == 0 || !seen.insert(s.pid) {
                        continue;
                    }
                    new_app_infos.push(AppInfo {
                        id: s.pid,
                        name: s.process_name.clone(),
                        volume: (s.volume * 100.0).round().clamp(0.0, 100.0) as u8,
                        muted: s.muted,
                    });
                }

                let old_pids: HashSet<u32> = app_infos.iter().map(|a| a.id).collect();
                let new_pids: HashSet<u32> = new_app_infos.iter().map(|a| a.id).collect();

                if old_pids != new_pids {
                    println!("sessions updated: {} → {} apps", app_infos.len(), new_app_infos.len());

                    // Extract icons for any brand-new apps, update volumes/mutes for all.
                    for s in &new_sessions {
                        if s.pid == 0 || !new_pids.contains(&s.pid) {
                            continue;
                        }
                        volumes.insert(s.pid, s.volume);
                        mutes.insert(s.pid, s.muted);
                        if !icons.contains_key(&s.pid) && !s.exe_path.is_empty() {
                            if let Some(px) = icons::extract_rgb565(&s.exe_path) {
                                println!("  extracted icon for {} pid={}", s.process_name, s.pid);
                                icons.insert(s.pid, px);
                            }
                        }
                    }

                    app_infos = new_app_infos.clone();
                    send(&mut *port, &HostToDevice::SetAppList(new_app_infos));
                }
            }
        }
    }
}

/// Send Ping and block until `DeviceToHost::Ready` or `Pong` arrives (or 3 s timeout).
/// Either message confirms the channel is clean. Any earlier decode errors are stale
/// bytes left in the FIFO from USB re-enumeration; they're discarded automatically
/// because we called `port.clear` + `decoder.reset` before sending the Ping.
fn wait_for_ready(port: &mut dyn serialport::SerialPort, decoder: &mut Decoder) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                let _ = decoder.push(&buf[..n]);
                loop {
                    match decoder.next_frame::<DeviceToHost>() {
                        Ok(Some(DeviceToHost::Ready { version })) => {
                            println!("device ready (protocol v{version})");
                            return;
                        }
                        Ok(Some(DeviceToHost::Pong)) => {
                            println!("device ready (pong)");
                            return;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {
                            decoder.reset();
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    println!("warn: timed out waiting for Ready, proceeding anyway");
}

pub fn send_and_wait(
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
                        Ok(Some(DeviceToHost::Ack)) => return,
                        Ok(Some(other)) => println!("<<< {other:?}"),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("<<< decode error: {e:?}");
                            let _ = port.clear(ClearBuffer::Input);
                            decoder.reset();
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

pub fn send(port: &mut dyn serialport::SerialPort, msg: &HostToDevice) {
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

pub fn drain_for(
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

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use heapless::String as HString;
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

/// Commands sent from the tray thread into the running worker.
pub enum WorkerCommand {
    ProvisionWifi {
        ssid: HString<32>,
        password: HString<64>,
        reply: std::sync::mpsc::SyncSender<Result<String, String>>,
    },
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

/// Unified transport interface over USB serial and TCP.
pub trait Transport: Read + Write {
    fn clear_input(&mut self);
    fn description(&self) -> String;
}

impl Transport for Box<dyn serialport::SerialPort> {
    fn clear_input(&mut self) {
        let _ = self.clear(ClearBuffer::Input);
    }
    fn description(&self) -> String {
        self.name().unwrap_or_default()
    }
}

/// TCP transport wrapping a `TcpStream`.
pub struct TcpTransport {
    stream: TcpStream,
    addr: String,
}

impl TcpTransport {
    pub fn connect(addr: &str) -> Result<Self, std::io::Error> {
        let sock_addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let stream = TcpStream::connect_timeout(&sock_addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;
        Ok(Self { stream, addr: addr.to_string() })
    }
}

impl Read for TcpTransport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for TcpTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl Transport for TcpTransport {
    fn clear_input(&mut self) {
        // TCP has no OS-level buffer clear; decoder reset handles recovery.
    }
    fn description(&self) -> String {
        self.addr.clone()
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
    cmd_rx: std::sync::mpsc::Receiver<WorkerCommand>,
) {
    let Some((app_infos, volumes, mutes, icon_map)) =
        prepare_audio_data(&status_tx) else { return };

    let mut transport: Box<dyn serialport::SerialPort> =
        match serialport::new(port_name, 115_200)
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

    // Allow USB CDC enumeration to settle before we start talking.
    std::thread::sleep(Duration::from_millis(200));
    run_inner(
        &mut transport,
        app_infos,
        volumes,
        mutes,
        icon_map,
        sensitivity,
        backlight,
        stop,
        &cmd_rx,
    );
}

/// Connect to the device via TCP and run the same event loop as `run`.
///
/// Retries the TCP connection every 3 seconds indefinitely (until `stop` is
/// set) to handle: companion starting before the device is on, and the device
/// being power-cycled after an established connection drops.
pub fn run_wifi(
    ip: &str,
    port: u16,
    sensitivity: Arc<AtomicU8>,
    backlight: Arc<BacklightShared>,
    stop: Arc<AtomicBool>,
    status_tx: std::sync::mpsc::Sender<WorkerStatus>,
    cmd_rx: std::sync::mpsc::Receiver<WorkerCommand>,
) {
    let Some((app_infos, volumes, mutes, icon_map)) =
        prepare_audio_data(&status_tx) else { return };

    let addr = format!("{ip}:{port}");

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        // Retry connecting every 3 s until success or stop.
        let mut transport = loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match TcpTransport::connect(&addr) {
                Ok(t) => break t,
                Err(e) => {
                    eprintln!("tcp connect to {addr} failed: {e}");
                    let _ = status_tx.send(WorkerStatus::Error(
                        "WiFi: connecting…".to_string(),
                    ));
                    let wait_until = Instant::now() + Duration::from_secs(3);
                    while Instant::now() < wait_until {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        };

        println!("connected to {addr}");
        let _ = status_tx.send(WorkerStatus::Connected(addr.clone()));

        run_inner(
            &mut transport,
            app_infos.clone(),
            volumes.clone(),
            mutes.clone(),
            icon_map.clone(),
            sensitivity.clone(),
            backlight.clone(),
            stop.clone(),
            &cmd_rx,
        );

        let _ = status_tx.send(WorkerStatus::Disconnected);

        if stop.load(Ordering::Relaxed) {
            return;
        }

        eprintln!("WiFi transport disconnected; reconnecting in 3 s…");
        let wait_until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < wait_until {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Send `SetWifiConfig` over USB and wait up to 30 s for the device to
/// connect and report its IP.  On success, saves the IP to config and
/// returns it.
pub fn provision_wifi(
    port_name: &str,
    ssid: &str,
    password: &str,
) -> Result<String, String> {
    let ssid_h: HString<32> =
        ssid.try_into().map_err(|_| "SSID too long (max 32 chars)".to_string())?;
    let pass_h: HString<64> =
        password.try_into().map_err(|_| "password too long (max 64 chars)".to_string())?;

    let mut port: Box<dyn serialport::SerialPort> =
        serialport::new(port_name, 115_200)
            .timeout(Duration::from_secs(2))
            .open()
            .map_err(|e| format!("port: {e}"))?;

    std::thread::sleep(Duration::from_millis(200));
    let mut decoder = Decoder::new(16 * 1024);
    let _ = drain_for(&mut port, Duration::from_millis(100), &mut decoder);
    port.clear_input();
    decoder = Decoder::new(16 * 1024);

    println!(">>> waiting for Ready...");
    send(&mut port, &HostToDevice::Ping);
    wait_for_ready(&mut port, &mut decoder);

    println!(">>> sending SetWifiConfig ssid={ssid}");
    send(&mut port, &HostToDevice::SetWifiConfig { ssid: ssid_h, password: pass_h });

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let _ = decoder.push(&buf[..n]);
                loop {
                    match decoder.next_frame::<DeviceToHost>() {
                        Ok(Some(DeviceToHost::WifiStatus { connected: true, ip })) => {
                            crate::config::save_wifi_ip(ip.as_str());
                            return Ok(ip.to_string());
                        }
                        Ok(Some(DeviceToHost::WifiStatus { connected: false, ip }))
                            if ip == "REBOOT" =>
                        {
                            return Err(
                                "device requires reboot to apply new credentials".to_string(),
                            );
                        }
                        Ok(Some(_)) | Ok(None) => break,
                        Err(_) => {
                            decoder.reset();
                            break;
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Err("timed out waiting for WiFi connection".to_string())
}

/// Enumerate audio sessions and build the data structures shared between
/// `run` and `run_wifi`.  Reports errors via `status_tx` and returns `None`
/// on failure.
fn prepare_audio_data(
    status_tx: &std::sync::mpsc::Sender<WorkerStatus>,
) -> Option<(
    Vec<AppInfo>,
    HashMap<u32, f32>,
    HashMap<u32, bool>,
    HashMap<u32, Vec<u8>>,
)> {
    if let Err(e) = audio::init() {
        eprintln!("COM init failed on worker thread: {e}");
        let _ = status_tx.send(WorkerStatus::Error(format!("COM: {e}")));
        return None;
    }

    let sessions = match audio::enumerate() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("audio enumerate failed: {e}");
            let _ = status_tx.send(WorkerStatus::Error(format!("audio: {e}")));
            return None;
        }
    };

    let mut app_infos: Vec<AppInfo> = Vec::new();
    let mut volumes: HashMap<u32, f32> = HashMap::new();
    let mut mutes: HashMap<u32, bool> = HashMap::new();
    let mut icon_map: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut seen_pids: HashSet<u32> = HashSet::new();

    for (idx, s) in sessions.iter().enumerate() {
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
                    icon_map.insert(id, px);
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
        return None;
    }

    Some((app_infos, volumes, mutes, icon_map))
}

/// Ready handshake + init sequence + main event loop.  Shared between USB
/// and WiFi transports.
fn run_inner(
    transport: &mut dyn Transport,
    app_infos: Vec<AppInfo>,
    mut volumes: HashMap<u32, f32>,
    mut mutes: HashMap<u32, bool>,
    mut icons: HashMap<u32, Vec<u8>>,
    sensitivity: Arc<AtomicU8>,
    backlight: Arc<BacklightShared>,
    stop: Arc<AtomicBool>,
    cmd_rx: &std::sync::mpsc::Receiver<WorkerCommand>,
) {
    let mut decoder = Decoder::new(16 * 1024);
    let _ = drain_for(transport, Duration::from_millis(100), &mut decoder);
    transport.clear_input();
    decoder = Decoder::new(16 * 1024);

    println!(">>> waiting for Ready (sending Ping)...");
    send(transport, &HostToDevice::Ping);
    wait_for_ready(transport, &mut decoder);

    let first_id = app_infos[0].id;
    println!(">>> sending SetAppList ({} apps)", app_infos.len());
    send_and_wait(transport, &mut decoder, &HostToDevice::SetAppList(app_infos.clone()));
    println!(">>> SetAppList ack'd");
    println!(">>> sending SetSelectedApp({first_id})");
    send_and_wait(transport, &mut decoder, &HostToDevice::SetSelectedApp(first_id));
    println!(">>> SetSelectedApp ack'd");
    for (&app_id, pixels) in &icons {
        println!(">>> sending SetAppIcon(app_id={app_id}, {}B)", pixels.len());
        send_and_wait(
            transport,
            &mut decoder,
            &HostToDevice::SetAppIcon { app_id, pixels: pixels.clone() },
        );
        println!(">>> SetAppIcon({app_id}) ack'd");
    }

    println!(">>> running — Ctrl+C or tray Exit to stop");
    let mut buf = [0u8; 256];
    let mut last_refresh = Instant::now();
    let mut last_heartbeat = Instant::now();
    // State for in-band WiFi provisioning (command arrives via cmd_rx).
    let mut pending_wifi_reply: Option<std::sync::mpsc::SyncSender<Result<String, String>>> = None;
    let mut wifi_deadline: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        match transport.read(&mut buf) {
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
                                    send(
                                        transport,
                                        &HostToDevice::SetVolume { app_id, level },
                                    );
                                }
                                Err(e) => eprintln!("set_volume({app_id}): {e}"),
                            }
                        }
                        Ok(Some(DeviceToHost::AppSelected(app_id))) => {
                            println!("<<< AppSelected({app_id})");
                            if let Some(&vol) = volumes.get(&app_id) {
                                let level = (vol * 100.0).round() as u8;
                                println!("  → SetVolume({app_id}, {level}%)");
                                send_and_wait(
                                    transport,
                                    &mut decoder,
                                    &HostToDevice::SetVolume { app_id, level },
                                );
                            } else {
                                println!("  → no volume entry for pid {app_id}");
                            }
                            if let Some(pixels) = icons.get(&app_id) {
                                let pixels = pixels.clone();
                                println!("  → SetAppIcon({app_id}, {}B)", pixels.len());
                                send_and_wait(
                                    transport,
                                    &mut decoder,
                                    &HostToDevice::SetAppIcon { app_id, pixels },
                                );
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
                                        transport,
                                        &HostToDevice::SetMute { app_id, muted: new_muted },
                                    );
                                }
                                Err(e) => eprintln!("set_mute({app_id}): {e}"),
                            }
                        }
                        Ok(Some(DeviceToHost::Ack)) => {}
                        Ok(Some(DeviceToHost::Pong)) => {}
                        Ok(Some(DeviceToHost::WifiStatus { connected, ip })) => {
                            println!("<<< WifiStatus connected={connected} ip={ip}");
                            if let Some(reply) = pending_wifi_reply.take() {
                                wifi_deadline = None;
                                if connected {
                                    crate::config::save_wifi_ip(ip.as_str());
                                    let _ = reply.send(Ok(ip.to_string()));
                                } else if ip.as_str() == "REBOOT" {
                                    let _ = reply.send(Err("device requires reboot to apply new WiFi credentials".to_string()));
                                } else {
                                    let _ = reply.send(Err("WiFi connection failed".to_string()));
                                }
                            }
                        }
                        Ok(Some(other)) => println!("<<< {other:?}"),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("<<< decode error: {e:?}");
                            transport.clear_input();
                            decoder.reset();
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

        if last_heartbeat.elapsed() >= Duration::from_secs(2) {
            last_heartbeat = Instant::now();
            send(transport, &HostToDevice::Ping);
        }

        if backlight.dirty.swap(false, Ordering::AcqRel) {
            send(transport, &backlight.snapshot());
        }

        // Handle commands from the tray thread (e.g. WiFi provisioning).
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::ProvisionWifi { ssid, password, reply } => {
                    send(transport, &HostToDevice::SetWifiConfig { ssid, password });
                    pending_wifi_reply = Some(reply);
                    wifi_deadline = Some(Instant::now() + Duration::from_secs(30));
                }
            }
        }
        // Time out a pending provision if the device never responded.
        if let Some(dl) = wifi_deadline {
            if Instant::now() > dl {
                if let Some(reply) = pending_wifi_reply.take() {
                    let _ = reply.send(Err("timed out waiting for WiFi connection".to_string()));
                }
                wifi_deadline = None;
            }
        }

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
                    println!(
                        "sessions updated: {} → {} apps",
                        app_infos.len(),
                        new_app_infos.len()
                    );

                    for s in &new_sessions {
                        if s.pid == 0 || !new_pids.contains(&s.pid) {
                            continue;
                        }
                        volumes.insert(s.pid, s.volume);
                        mutes.insert(s.pid, s.muted);
                        if !icons.contains_key(&s.pid) && !s.exe_path.is_empty() {
                            if let Some(px) = icons::extract_rgb565(&s.exe_path) {
                                println!(
                                    "  extracted icon for {} pid={}",
                                    s.process_name, s.pid
                                );
                                icons.insert(s.pid, px);
                            }
                        }
                    }

                    send(transport, &HostToDevice::SetAppList(new_app_infos));
                }
            }
        }
    }
}

/// Send Ping and block until `DeviceToHost::Ready` or `Pong` arrives (or 3 s timeout).
fn wait_for_ready(transport: &mut dyn Transport, decoder: &mut Decoder) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match transport.read(&mut buf) {
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
    transport: &mut dyn Transport,
    decoder: &mut Decoder,
    msg: &HostToDevice,
) {
    send(transport, msg);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match transport.read(&mut buf) {
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
                            transport.clear_input();
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

pub fn send(transport: &mut dyn Transport, msg: &HostToDevice) {
    let bytes = match codec::encode(msg) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("encode error: {e:?}");
            return;
        }
    };
    if let Err(e) = transport.write_all(&bytes) {
        eprintln!("write error: {e}");
    }
}

pub fn drain_for(
    transport: &mut dyn Transport,
    duration: Duration,
    decoder: &mut Decoder,
) -> usize {
    let deadline = Instant::now() + duration;
    let mut buf = [0u8; 256];
    let mut total = 0;
    while Instant::now() < deadline {
        match transport.read(&mut buf) {
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

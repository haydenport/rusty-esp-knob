extern crate alloc;

use alloc::vec::Vec;
use core::mem;

use esp_hal::{rng::Rng, timer::timg::TimerGroup};
use esp_wifi::{
    EspWifiController,
    wifi::{AuthMethod, ClientConfiguration, Configuration, WifiController, WifiDevice},
};
use smoltcp::{
    iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet},
    socket::{
        dhcpv4,
        dhcpv4::Socket as DhcpSocket,
        tcp::{Socket as TcpSocket, SocketBuffer},
    },
    time::Instant,
    wire::{EthernetAddress, IpCidr, Ipv4Address, Ipv4Cidr},
};
use static_cell::StaticCell;

use protocol::codec::{self, Decoder};
use protocol::messages::{DeviceToHost, HostToDevice};

const TCP_PORT: u16 = 9000;
const MAX_FRAME_LEN: usize = 12 * 1024;

static WIFI_INIT_CELL: StaticCell<EspWifiController<'static>> = StaticCell::new();

static mut TCP_RX_BUF: [u8; 4096] = [0; 4096];
static mut TCP_TX_BUF: [u8; 2048] = [0; 2048];

#[derive(Clone, Copy, PartialEq)]
enum WifiTcpState {
    Connecting,
    WaitingForIp,
    Listening,
    Active,
}

pub struct WifiTcp {
    device: WifiDevice<'static>,
    controller: WifiController<'static>,
    iface: Interface,
    sockets: SocketSet<'static>,
    tcp_handle: SocketHandle,
    dhcp_handle: SocketHandle,
    decoder: Decoder,
    state: WifiTcpState,
    pub connected_ip: Option<heapless::String<16>>,
    pending_event: Option<DeviceToHost>,
    /// Timestamp when we entered the current Connecting/WaitingForIp state.
    /// Used to drive the retry timeout.
    phase_started_ms: u64,
}

/// Initialise the WiFi stack and return a `WifiTcp` instance.
///
/// Returns `None` if any initialisation step fails (e.g. bad SSID/password
/// length, or if `esp_wifi::init` panics — which it would on a clock too
/// low, but the ESP32-S3 default clock is well above the 80 MHz minimum).
///
/// # Safety
/// Transmutes peripheral lifetimes to `'static`.  This is valid because
/// `main()` is `-> !` and hardware registers are never freed.
pub fn init_wifi(
    timg1: esp_hal::peripherals::TIMG1,
    wifi_periph: esp_hal::peripherals::WIFI,
    ssid: &str,
    password: &str,
) -> Option<WifiTcp> {
    let tg = TimerGroup::new(timg1);
    let timer: esp_hal::timer::timg::Timer<'static> = unsafe { mem::transmute(tg.timer0) };
    let wifi_periph: esp_hal::peripherals::WIFI<'static> =
        unsafe { mem::transmute(wifi_periph) };

    let init = WIFI_INIT_CELL.init(esp_wifi::init(timer, Rng::new()).ok()?);

    let (mut controller, interfaces) = esp_wifi::wifi::new(init, wifi_periph).ok()?;

    controller
        .set_configuration(&Configuration::Client(ClientConfiguration {
            ssid: ssid.try_into().ok()?,
            password: password.try_into().ok()?,
            auth_method: AuthMethod::WPA2Personal,
            ..Default::default()
        }))
        .ok()?;
    controller.start().ok()?;
    controller.connect().ok()?;

    // Extend lifetimes to 'static — safe because main() never exits.
    let controller: WifiController<'static> = unsafe { mem::transmute(controller) };
    let mut device: WifiDevice<'static> = unsafe { mem::transmute(interfaces.sta) };

    let mac = device.mac_address();
    let mut iface_cfg = IfaceConfig::new(EthernetAddress(mac).into());
    iface_cfg.random_seed = 0xdead_beef_cafe_0000;

    let mut iface = Interface::new(iface_cfg, &mut device, Instant::ZERO);
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(Ipv4Address::UNSPECIFIED.into(), 0));
    });

    let mut sockets: SocketSet<'static> = SocketSet::new(Vec::new());
    let dhcp_handle = sockets.add(DhcpSocket::new());
    // SAFETY: TCP_RX_BUF and TCP_TX_BUF are 'static arrays, accessed only here.
    let tcp = TcpSocket::new(
        SocketBuffer::new(unsafe { &mut TCP_RX_BUF[..] }),
        SocketBuffer::new(unsafe { &mut TCP_TX_BUF[..] }),
    );
    let tcp_handle = sockets.add(tcp);

    Some(WifiTcp {
        device,
        controller,
        iface,
        sockets,
        tcp_handle,
        dhcp_handle,
        decoder: Decoder::new(MAX_FRAME_LEN),
        state: WifiTcpState::Connecting,
        connected_ip: None,
        pending_event: None,
        phase_started_ms: 0,
    })
}

impl WifiTcp {
    /// Drive the WiFi + smoltcp state machine for one tick.
    ///
    /// Returns `Some(msg)` when a complete, validated frame arrives over TCP.
    /// Returns `None` on every tick where no incoming frame is ready.
    pub fn poll(&mut self, timestamp_ms: u64) -> Option<HostToDevice> {
        let now = Instant::from_millis(timestamp_ms as i64);
        self.iface.poll(now, &mut self.device, &mut self.sockets);

        match self.state {
            WifiTcpState::Connecting => {
                if self.controller.is_connected().unwrap_or(false) {
                    self.phase_started_ms = timestamp_ms;
                    self.state = WifiTcpState::WaitingForIp;
                } else if timestamp_ms.saturating_sub(self.phase_started_ms) > 10_000 {
                    // No association in 10 s — retry connect().
                    self.phase_started_ms = timestamp_ms;
                    let _ = self.controller.connect();
                }
            }
            WifiTcpState::WaitingForIp => {
                // Extract only Copy values from the DHCP event before
                // releasing the sockets borrow, so we can safely access
                // self.iface and self.state afterwards.
                let dhcp_result = {
                    let socket = self.sockets.get_mut::<DhcpSocket>(self.dhcp_handle);
                    match socket.poll() {
                        None => None,
                        Some(dhcpv4::Event::Configured(cfg)) => {
                            Some((true, Some((cfg.address, cfg.router))))
                        }
                        Some(dhcpv4::Event::Deconfigured) => Some((false, None)),
                    }
                };
                if let Some((configured, details)) = dhcp_result {
                    if configured {
                        let (addr, router) = details.unwrap();
                        self.iface.update_ip_addrs(|addrs| {
                            let cidr = IpCidr::Ipv4(Ipv4Cidr::new(
                                addr.address(),
                                addr.prefix_len(),
                            ));
                            if addrs.is_empty() {
                                let _ = addrs.push(cidr);
                            } else {
                                addrs[0] = cidr;
                            }
                        });
                        if let Some(router_addr) = router {
                            let _ = self
                                .iface
                                .routes_mut()
                                .add_default_ipv4_route(router_addr);
                        }
                        let ip = addr.address();
                        let octets = ip.octets();
                        let mut ip_str = heapless::String::<16>::new();
                        let _ = core::fmt::write(
                            &mut ip_str,
                            format_args!(
                                "{}.{}.{}.{}",
                                octets[0], octets[1], octets[2], octets[3]
                            ),
                        );
                        self.connected_ip = Some(ip_str.clone());
                        self.pending_event = Some(DeviceToHost::WifiStatus {
                            connected: true,
                            ip: ip_str,
                        });
                        self.state = WifiTcpState::Listening;
                    } else {
                        // DHCP deconfigured — go back to connecting.
                        self.iface.update_ip_addrs(|addrs| addrs.clear());
                        let _ = self.iface.routes_mut().remove_default_ipv4_route();
                        self.connected_ip = None;
                        self.phase_started_ms = timestamp_ms;
                        self.state = WifiTcpState::Connecting;
                        let _ = self.controller.connect();
                    }
                } else if timestamp_ms.saturating_sub(self.phase_started_ms) > 15_000 {
                    // Associated but no DHCP lease in 15 s — retry from scratch.
                    self.phase_started_ms = timestamp_ms;
                    self.state = WifiTcpState::Connecting;
                    let _ = self.controller.connect();
                }
            }
            WifiTcpState::Listening => {
                let socket = self.sockets.get_mut::<TcpSocket>(self.tcp_handle);
                if !socket.is_listening() && !socket.is_active() {
                    let _ = socket.listen(TCP_PORT);
                }
                if socket.is_active() {
                    self.state = WifiTcpState::Active;
                }
            }
            WifiTcpState::Active => {
                let active = self
                    .sockets
                    .get_mut::<TcpSocket>(self.tcp_handle)
                    .is_active();
                if !active {
                    self.sockets
                        .get_mut::<TcpSocket>(self.tcp_handle)
                        .abort();
                    self.state = WifiTcpState::Listening;
                    return None;
                }
                // Drain up to 256 bytes from the TCP RX buffer into the frame
                // decoder. Using a fixed stack scratch avoids touching the heap
                // on every tick.
                let mut scratch = [0u8; 256];
                let n = {
                    let socket = self.sockets.get_mut::<TcpSocket>(self.tcp_handle);
                    let mut received = 0usize;
                    let _ = socket.recv(|data| {
                        let len = data.len().min(scratch.len());
                        scratch[..len].copy_from_slice(&data[..len]);
                        received = len;
                        (len, ())
                    });
                    received
                };
                if n > 0 {
                    if self.decoder.push(&scratch[..n]).is_err() {
                        self.decoder.reset();
                    }
                    if let Ok(Some(msg)) = self.decoder.next_frame::<HostToDevice>() {
                        return Some(msg);
                    }
                }
            }
        }
        None
    }

    /// Consume and return any pending WiFi status event (e.g. IP acquired).
    ///
    /// Returns `Some` once after the IP is first obtained, then `None` until
    /// the next connectivity change.
    pub fn take_event(&mut self) -> Option<DeviceToHost> {
        self.pending_event.take()
    }

    /// Encode and send a message to the connected TCP client.
    ///
    /// Silently drops the message if no client is connected or if the TCP
    /// send buffer is full.
    pub fn send(&mut self, msg: &DeviceToHost) {
        if self.state != WifiTcpState::Active {
            return;
        }
        if let Ok(bytes) = codec::encode(msg) {
            let socket = self.sockets.get_mut::<TcpSocket>(self.tcp_handle);
            if socket.can_send() {
                let _ = socket.send_slice(&bytes);
            }
        }
    }

    /// Returns `true` while a TCP client is connected.
    pub fn is_active(&self) -> bool {
        self.state == WifiTcpState::Active
    }
}

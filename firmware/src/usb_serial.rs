//! USB Serial/JTAG transport for the protocol layer.
//!
//! Wraps `esp_hal::usb_serial_jtag::UsbSerialJtag` with a streaming
//! [`Decoder`] so callers can `poll()` for complete messages and `send()`
//! typed messages that get postcard-serialized + CRC'd + COBS-framed.

use esp_hal::usb_serial_jtag::UsbSerialJtag;
use esp_hal::Blocking;
use protocol::codec::{self, Decoder};
use protocol::messages::{DeviceToHost, HostToDevice};

/// Upper bound on a single decoded frame, in bytes.
///
/// Sized for a 64×64 RGB565 icon (≈ 8 KB pixels + postcard/COBS/CRC
/// overhead). The Decoder allocates this much up-front via `Vec::with_capacity`,
/// so it's a fixed steady-state cost. Don't raise without recalculating peak
/// heap during a SetAppIcon swap (decoder + old icon capacity + incoming
/// pixels Vec): we only have ~37 KB free after the framebuffer.
const MAX_FRAME_LEN: usize = 12 * 1024;

/// Diagnostic counters — displayed on screen so we can see what the transport
/// is actually doing without any log output.
#[derive(Debug, Default, Clone, Copy)]
pub struct RxStats {
    pub bytes: u32,
    pub ok: u32,
    pub err: u32,
    pub overflow: u32,
    /// Count of outgoing frames dropped because the TX FIFO stayed full past
    /// the per-byte spin budget. Happens when the host isn't reading (e.g.
    /// the companion has exited).
    pub tx_drop: u32,
}

/// Per-`poll()` read budget. Larger = fewer iterations to drain a big icon
/// push (8 KB icon takes ~16 polls at 512 B vs ~64 at 128 B), but each poll
/// still completes well under 1 ms of work so the main loop stays responsive.
const SCRATCH_LEN: usize = 512;

pub struct UsbSerial<'d> {
    jtag: UsbSerialJtag<'d, Blocking>,
    decoder: Decoder,
    scratch: [u8; SCRATCH_LEN],
    stats: RxStats,
}

impl<'d> UsbSerial<'d> {
    pub fn new(jtag: UsbSerialJtag<'d, Blocking>) -> Self {
        Self {
            jtag,
            decoder: Decoder::new(MAX_FRAME_LEN),
            scratch: [0; SCRATCH_LEN],
            stats: RxStats::default(),
        }
    }

    pub fn stats(&self) -> RxStats {
        self.stats
    }

    /// Drain the RX FIFO and try to decode the next message.
    ///
    /// Returns `Some(msg)` once a complete, CRC-valid frame has arrived.
    /// Call repeatedly — multiple frames may already be buffered. Reads at
    /// most `scratch.len()` bytes per call so the main loop always makes
    /// progress, even when the host is pushing bytes continuously. (An
    /// earlier revision drained in a loop until the FIFO was empty; under a
    /// sustained ~8 KB icon push the loop never exited, starving encoder
    /// and display updates.)
    pub fn poll(&mut self) -> Option<HostToDevice> {
        let mut n = 0;
        while n < self.scratch.len() {
            match self.jtag.read_byte() {
                Ok(b) => {
                    self.scratch[n] = b;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        if n > 0 {
            self.stats.bytes = self.stats.bytes.saturating_add(n as u32);
            if self.decoder.push(&self.scratch[..n]).is_err() {
                self.stats.overflow = self.stats.overflow.saturating_add(1);
            }
        }
        match self.decoder.next_frame() {
            Ok(Some(msg)) => {
                self.stats.ok = self.stats.ok.saturating_add(1);
                Some(msg)
            }
            Ok(None) => None,
            Err(_) => {
                self.stats.err = self.stats.err.saturating_add(1);
                None
            }
        }
    }

    /// Serialize + frame + write a message to the host.
    ///
    /// Non-blocking: the IN FIFO is written byte-by-byte with a bounded
    /// spin budget per byte. If the host isn't draining the FIFO (e.g. the
    /// companion has exited), the frame is dropped and `tx_drop` is bumped.
    /// The blocking `UsbSerialJtag::write` would spin forever in that
    /// case, wedging the main loop the next time we try to send an event.
    pub fn send(&mut self, msg: &DeviceToHost) -> Result<(), codec::Error> {
        const SPIN_BUDGET: u32 = 20_000;
        let bytes = codec::encode(msg)?;
        for &b in &bytes {
            let mut tries = 0u32;
            loop {
                match self.jtag.write_byte_nb(b) {
                    Ok(()) => break,
                    Err(_) => {
                        tries += 1;
                        if tries >= SPIN_BUDGET {
                            self.stats.tx_drop = self.stats.tx_drop.saturating_add(1);
                            return Ok(());
                        }
                    }
                }
            }
        }
        // Commit the short packet. If flush won't complete right now, that's
        // fine — hardware auto-flushes on the next full 64-byte packet or
        // when the host next polls.
        let _ = self.jtag.flush_tx_nb();
        Ok(())
    }
}

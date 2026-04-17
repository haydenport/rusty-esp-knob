//! USB Serial/JTAG transport for the protocol layer.
//!
//! Wraps `esp_hal::usb_serial_jtag::UsbSerialJtag` with a streaming
//! [`Decoder`] so callers can `poll()` for complete messages and `send()`
//! typed messages that get postcard-serialized + CRC'd + COBS-framed.

use esp_hal::usb_serial_jtag::UsbSerialJtag;
use esp_hal::Blocking;
use protocol::codec::{self, Decoder};
use protocol::messages::{DeviceToHost, HostToDevice};

/// Bounded read buffer. Small for now — bump when SetAppIcon lands in Phase 6
/// (a 64×64 RGB565 icon is ~8 KB, but that requires a bigger heap first).
const MAX_FRAME_LEN: usize = 1024;

pub struct UsbSerial<'d> {
    jtag: UsbSerialJtag<'d, Blocking>,
    decoder: Decoder,
    scratch: [u8; 128],
}

impl<'d> UsbSerial<'d> {
    pub fn new(jtag: UsbSerialJtag<'d, Blocking>) -> Self {
        Self {
            jtag,
            decoder: Decoder::new(MAX_FRAME_LEN),
            scratch: [0; 128],
        }
    }

    /// Drain the RX FIFO and try to decode the next message.
    ///
    /// Returns `Some(msg)` once a complete, CRC-valid frame has arrived.
    /// Call repeatedly — multiple frames may already be buffered.
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
            let _ = self.decoder.push(&self.scratch[..n]);
        }
        match self.decoder.next_frame() {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    /// Serialize + frame + write a message to the host.
    pub fn send(&mut self, msg: &DeviceToHost) -> Result<(), codec::Error> {
        let bytes = codec::encode(msg)?;
        self.jtag.write(&bytes).map_err(|_| codec::Error::Serialize)?;
        Ok(())
    }
}

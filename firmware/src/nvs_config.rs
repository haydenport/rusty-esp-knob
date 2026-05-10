use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;

/// Byte layout within the NVS partition (starting at NVS_OFFSET).
///   [0..2]   magic 0xBEEF — confirms a valid credential block
///   [2..34]  SSID (32 bytes, null-padded)
///   [34..98] password (64 bytes, null-padded)
///   [98..100] padding (zero) — rounds up to 4-byte write alignment
///
/// The NVS partition starts at 0x9000 in the default ESP-IDF partition table.
const NVS_OFFSET: u32 = 0x9000;
const MAGIC: u16 = 0xBEEF;
const SSID_LEN: usize = 32;
const PASS_LEN: usize = 64;
/// Padded to a multiple of 4 (NorFlash WORD_SIZE requirement).
const RECORD_LEN: usize = 100;

pub struct WifiCredentials {
    pub ssid: heapless::String<32>,
    pub password: heapless::String<64>,
}

pub struct NvsConfig<'d> {
    storage: FlashStorage<'d>,
}

impl<'d> NvsConfig<'d> {
    pub fn new(flash: esp_hal::peripherals::FLASH<'d>) -> Self {
        Self {
            storage: FlashStorage::new(flash),
        }
    }

    /// Read WiFi credentials from flash. Returns `None` if no valid record exists.
    pub fn load(&mut self) -> Option<WifiCredentials> {
        let mut buf = [0u8; RECORD_LEN];
        ReadNorFlash::read(&mut self.storage, NVS_OFFSET, &mut buf).ok()?;

        if u16::from_le_bytes([buf[0], buf[1]]) != MAGIC {
            return None;
        }

        let ssid_raw = &buf[2..2 + SSID_LEN];
        let pass_raw = &buf[2 + SSID_LEN..2 + SSID_LEN + PASS_LEN];

        let ssid_len = ssid_raw.iter().position(|&b| b == 0).unwrap_or(SSID_LEN);
        let pass_len = pass_raw.iter().position(|&b| b == 0).unwrap_or(PASS_LEN);

        let mut ssid = heapless::String::<32>::new();
        let mut password = heapless::String::<64>::new();

        for &b in &ssid_raw[..ssid_len] {
            ssid.push(b as char).ok()?;
        }
        for &b in &pass_raw[..pass_len] {
            password.push(b as char).ok()?;
        }

        Some(WifiCredentials { ssid, password })
    }

    /// Write WiFi credentials to flash.
    ///
    /// Erases the 4 KB sector at NVS_OFFSET first, then writes the record.
    pub fn save(&mut self, creds: &WifiCredentials) -> Result<(), ()> {
        // Erase one sector (4096 bytes, the minimum erasable unit).
        NorFlash::erase(&mut self.storage, NVS_OFFSET, NVS_OFFSET + 4096)
            .map_err(|_| ())?;

        let mut buf = [0u8; RECORD_LEN];
        buf[0..2].copy_from_slice(&MAGIC.to_le_bytes());

        let ssid_b = creds.ssid.as_bytes();
        buf[2..2 + ssid_b.len()].copy_from_slice(ssid_b);

        let pass_b = creds.password.as_bytes();
        buf[2 + SSID_LEN..2 + SSID_LEN + pass_b.len()].copy_from_slice(pass_b);

        NorFlash::write(&mut self.storage, NVS_OFFSET, &buf).map_err(|_| ())
    }
}

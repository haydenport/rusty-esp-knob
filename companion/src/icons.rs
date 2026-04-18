//! Extract a 64×64 app icon from an executable and convert it to the
//! big-endian RGB565 pixel format the firmware's `ImageRaw` renderer expects.
//!
//! Pipeline: `PrivateExtractIconsW` pulls a raw HICON out of the exe's
//! resource table, `GetIconInfo` gives us the color HBITMAP, `GetDIBits`
//! copies pixels into a BGRA buffer, and we fold that into RGB565.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, PrivateExtractIconsW, HICON, ICONINFO,
};

pub const ICON_SIZE: u32 = 64;

/// Extract the 64×64 icon from `exe_path` and return RGB565 big-endian pixel
/// bytes (length = 64 * 64 * 2 = 8192). Alpha-blended against black so
/// transparent pixels show as background color on the AMOLED.
pub fn extract_rgb565(exe_path: &str) -> Option<Vec<u8>> {
    let bgra = extract_bgra(exe_path)?;
    Some(bgra_to_rgb565_be(&bgra, ICON_SIZE, ICON_SIZE))
}

/// Debug helper: dump the extracted icon (after BGRA→RGB blend) to a PPM file
/// so we can visually verify the extraction step is working.
pub fn dump_ppm(exe_path: &str, out_path: &str) -> std::io::Result<()> {
    use std::io::Write;
    let bgra = match extract_bgra(exe_path) {
        Some(b) => b,
        None => return Err(std::io::Error::new(std::io::ErrorKind::Other, "no icon")),
    };
    let has_alpha = (0..(ICON_SIZE * ICON_SIZE) as usize).any(|i| bgra[i * 4 + 3] != 0);
    let mut f = std::fs::File::create(out_path)?;
    writeln!(f, "P6")?;
    writeln!(f, "{} {}", ICON_SIZE, ICON_SIZE)?;
    writeln!(f, "255")?;
    for i in 0..(ICON_SIZE * ICON_SIZE) as usize {
        let b = bgra[i * 4] as u16;
        let g = bgra[i * 4 + 1] as u16;
        let r = bgra[i * 4 + 2] as u16;
        let a = bgra[i * 4 + 3] as u16;
        let (r, g, b) = if has_alpha {
            (r * a / 255, g * a / 255, b * a / 255)
        } else {
            (r, g, b)
        };
        f.write_all(&[r as u8, g as u8, b as u8])?;
    }
    Ok(())
}

fn extract_bgra(exe_path: &str) -> Option<Vec<u8>> {
    unsafe {
        // The windows-rs binding wraps szfilename as `&[u16; 260]` — copy our
        // path (plus terminator) into a fixed-size buffer.
        let mut path_buf = [0u16; 260];
        for (i, w) in OsStr::new(exe_path)
            .encode_wide()
            .take(path_buf.len() - 1)
            .enumerate()
        {
            path_buf[i] = w;
        }

        let mut icons = [HICON::default(); 1];
        let mut icon_id: u32 = 0;

        let extracted = PrivateExtractIconsW(
            &path_buf,
            0,
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            Some(&mut icons),
            Some(&mut icon_id),
            0,
        );

        if extracted == 0 || extracted == u32::MAX || icons[0].is_invalid() {
            return None;
        }

        let result = hicon_to_bgra(icons[0]);
        let _ = DestroyIcon(icons[0]);
        result
    }
}

unsafe fn hicon_to_bgra(hicon: HICON) -> Option<Vec<u8>> {
    let mut info = ICONINFO::default();
    if GetIconInfo(hicon, &mut info).is_err() {
        return None;
    }

    // GetIconInfo allocates a color (and mask) HBITMAP we must free.
    let hbm_color = info.hbmColor;
    let hbm_mask = info.hbmMask;

    let screen_dc = GetDC(HWND::default());
    let mem_dc = CreateCompatibleDC(screen_dc);

    let width = ICON_SIZE as i32;
    let height = ICON_SIZE as i32;

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height => top-down DIB (origin at top-left).
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut buf = vec![0u8; (width * height * 4) as usize];
    let scan_lines = GetDIBits(
        mem_dc,
        hbm_color,
        0,
        height as u32,
        Some(buf.as_mut_ptr() as *mut _),
        &mut bmi,
        DIB_RGB_COLORS,
    );

    let _ = DeleteDC(mem_dc);
    ReleaseDC(HWND::default(), screen_dc);
    let _ = DeleteObject(hbm_color);
    if !hbm_mask.is_invalid() {
        let _ = DeleteObject(hbm_mask);
    }

    if scan_lines == 0 {
        return None;
    }

    Some(buf)
}

/// Convert a top-down BGRA8888 buffer to big-endian RGB565, alpha-blended
/// against a black background.
///
/// Some icons (legacy 24-bit or poorly authored) come back from `GetDIBits`
/// with the alpha channel all zeros — naively premultiplying would blank the
/// whole image. Detect that and treat the icon as fully opaque.
fn bgra_to_rgb565_be(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixels = (width * height) as usize;

    let has_alpha = (0..pixels).any(|i| bgra[i * 4 + 3] != 0);

    let mut out = Vec::with_capacity(pixels * 2);
    for i in 0..pixels {
        let b = bgra[i * 4] as u16;
        let g = bgra[i * 4 + 1] as u16;
        let r = bgra[i * 4 + 2] as u16;
        let a = bgra[i * 4 + 3] as u16;

        let (r, g, b) = if has_alpha {
            (r * a / 255, g * a / 255, b * a / 255)
        } else {
            (r, g, b)
        };

        let rgb565 = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | ((b & 0xF8) >> 3);
        out.push((rgb565 >> 8) as u8);
        out.push((rgb565 & 0xFF) as u8);
    }

    out
}

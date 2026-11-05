//! Instant insert (Shift+Enter): paste a meme straight into the app you were using,
//! then put your clipboard back as it was.
//!
//! No OS lets one app drop an image into another app's text box directly, so this does
//! what Raycast / PowerToys-style tools do:
//!   1. in ONE clipboard session: save everything on the clipboard, put the meme on
//!      (excluded from Win+V history / cloud clipboard), and close,
//!   2. hide the pane, refocus the app you were in, and send Ctrl+V,
//!   3. a moment later, restore what was there, unless something else has been copied
//!      in the meantime.
//!
//! Like Copy, still images go on as pixels (PNG + bitmap) and animated memes as the
//! file itself, so GIFs keep their animation.

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use std::path::Path;
use std::time::Duration;
use tauri::AppHandle;

/// Give the OS a moment to hand focus back to the target app before pasting.
const FOCUS_SETTLE: Duration = Duration::from_millis(90);
/// How long the target app gets to read the meme off the clipboard before we restore.
#[cfg(windows)]
const RESTORE_AFTER: Duration = Duration::from_millis(800);

/// `target` is the window that was in front before the pane opened (0 = unknown).
#[cfg(windows)]
pub fn insert(app: &AppHandle, path: &Path, target: isize) -> Result<(), String> {
    // Prepare everything first: if this fails, the clipboard hasn't been touched.
    let content = if crate::clipboard::is_animated(path) {
        win::Content::File(path.to_path_buf())
    } else {
        let (png, dib) = encode(path)?;
        win::Content::Image { png, dib }
    };

    let saved = win::swap_in(&content)?;
    let our_seq = win::sequence();

    crate::hide_pane(app);
    win::focus(target);
    std::thread::sleep(FOCUS_SETTLE);
    let pasted = send_paste();
    let paste_sent = pasted.is_ok();

    std::thread::spawn(move || {
        // If the paste never went out, restore straight away.
        if paste_sent {
            std::thread::sleep(RESTORE_AFTER);
        }
        // A different sequence number means something new was copied: keep that.
        if win::sequence() == our_seq {
            win::restore(&saved);
        }
    });
    pasted
}

/// Other platforms: paste via the clipboard, without restoring it yet.
/// (macOS needs Accessibility permission for the simulated Cmd+V.)
#[cfg(not(windows))]
pub fn insert(app: &AppHandle, path: &Path, _target: isize) -> Result<(), String> {
    crate::clipboard::copy(path)?;
    crate::hide_pane(app);
    std::thread::sleep(FOCUS_SETTLE);
    send_paste()
}

/// The meme as PNG bytes (keeps transparency; preferred by most apps) and as a 32-bit
/// CF_DIB (for everything else).
#[cfg(windows)]
fn encode(path: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let img = crate::clipboard::decode(path)?;
    let (w, h) = img.dimensions();

    // Already a PNG? Use the original bytes instead of re-encoding.
    let original = std::fs::read(path).map_err(|e| e.to_string())?;
    let png = if original.starts_with(b"\x89PNG\r\n\x1a\n") {
        original
    } else {
        use image::ImageEncoder;
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut out,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Sub,
        )
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("Couldn't encode image: {e}"))?;
        out
    };

    // BITMAPINFOHEADER (40 bytes), then bottom-up BGRA rows.
    let mut dib = Vec::with_capacity(40 + (w * h * 4) as usize);
    for field in [40u32, w, h] {
        dib.extend_from_slice(&field.to_le_bytes()); // biSize, biWidth, biHeight
    }
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&(w * h * 4).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&[0u8; 16]); // resolution, colours used/important
    for row in img.rows().rev() {
        for px in row {
            let [r, g, b, a] = px.0;
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok((png, dib))
}

fn send_paste() -> Result<(), String> {
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| format!("Couldn't simulate keys: {e}"))?;
    let err = |e: enigo::InputError| format!("Couldn't send paste: {e}");

    // Insert is triggered with Shift held, and Ctrl+Shift+V means "paste as plain text"
    // in Discord, Slack, browsers... which drops images. Let go of Shift first.
    for shift in [Key::Shift, Key::LShift, Key::RShift] {
        let _ = enigo.key(shift, Direction::Release);
    }

    #[cfg(windows)]
    let (modifier, v) = (Key::Control, Key::V);
    #[cfg(target_os = "macos")]
    let (modifier, v) = (Key::Meta, Key::Unicode('v'));
    #[cfg(not(any(windows, target_os = "macos")))]
    let (modifier, v) = (Key::Control, Key::Unicode('v'));

    enigo.key(modifier, Direction::Press).map_err(err)?;
    let result = enigo.key(v, Direction::Click).map_err(err);
    // Always release the modifier, even if V failed.
    enigo.key(modifier, Direction::Release).map_err(err)?;
    result
}

/// Win32 clipboard snapshot / swap / restore, and focus handling.
#[cfg(windows)]
pub mod win {
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{GlobalFree, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardSequenceNumber,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow, SetForegroundWindow};

    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;
    /// Skip anything bigger than this (e.g. a giant copied image) rather than stall.
    const MAX_FORMAT_BYTES: usize = 64 * 1024 * 1024;

    pub enum Content {
        Image { png: Vec<u8>, dib: Vec<u8> },
        File(PathBuf),
    }

    pub struct Snapshot(Vec<(u32, Vec<u8>)>);

    /// The current foreground window, as an integer we can keep in an atomic.
    pub fn foreground() -> isize {
        unsafe { GetForegroundWindow() as isize }
    }

    pub fn focus(target: isize) {
        let hwnd = target as HWND;
        if target != 0 && unsafe { IsWindow(hwnd) } != 0 {
            unsafe { SetForegroundWindow(hwnd) };
        }
    }

    pub fn sequence() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    /// Formats holding GDI/other handles can't be copied byte-for-byte. Windows
    /// re-synthesizes bitmaps from CF_DIB, which we keep.
    fn is_memory_format(format: u32) -> bool {
        const CF_BITMAP: u32 = 2;
        const CF_METAFILEPICT: u32 = 3;
        const CF_PALETTE: u32 = 9;
        const CF_ENHMETAFILE: u32 = 14;
        !matches!(format, CF_BITMAP | CF_METAFILEPICT | CF_PALETTE | CF_ENHMETAFILE | 0x80..=0x8F | 0x200..=0x3FF)
    }

    /// Opens the clipboard, retrying for ~1s while another app holds it.
    fn open() -> Result<(), String> {
        for _ in 0..50 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err("The clipboard is busy (another app has it open). Try again in a moment.".into())
    }

    /// Reads every memory-backed format. The clipboard must already be open.
    unsafe fn read_all() -> Snapshot {
        let mut formats = Vec::new();
        let mut format = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if !is_memory_format(format) {
                continue;
            }
            unsafe {
                let handle = GetClipboardData(format);
                if handle.is_null() {
                    continue;
                }
                let size = GlobalSize(handle);
                if size == 0 || size > MAX_FORMAT_BYTES {
                    continue;
                }
                let ptr = GlobalLock(handle) as *const u8;
                if ptr.is_null() {
                    continue;
                }
                formats.push((format, std::slice::from_raw_parts(ptr, size).to_vec()));
                GlobalUnlock(handle);
            }
        }
        Snapshot(formats)
    }

    fn register(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
    }

    /// Hands `bytes` to the open clipboard under `format`; the clipboard owns them on success.
    unsafe fn put(format: u32, bytes: &[u8]) -> bool {
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
            if handle.is_null() {
                return false;
            }
            let ptr = GlobalLock(handle) as *mut u8;
            if ptr.is_null() {
                GlobalFree(handle);
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            GlobalUnlock(handle);
            if SetClipboardData(format, handle).is_null() {
                GlobalFree(handle);
                return false;
            }
            true
        }
    }

    /// CF_HDROP payload: a DROPFILES header, then the UTF-16 path, double-NUL terminated.
    pub(super) fn hdrop(path: &std::path::Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&20u32.to_le_bytes()); // pFiles: offset of the path list
        bytes.extend_from_slice(&[0u8; 8]); // pt (POINT)
        bytes.extend_from_slice(&0u32.to_le_bytes()); // fNC
        bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide: UTF-16 paths
        for unit in path.as_os_str().to_string_lossy().encode_utf16().chain([0, 0]) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    /// Keeps what we put on the clipboard out of Win+V history, cloud clipboard and clipboard managers.
    unsafe fn exclude_from_history() {
        unsafe {
            put(register("ExcludeClipboardContentFromMonitorProcessing"), &[0]);
            put(register("CanIncludeInClipboardHistory"), &0u32.to_le_bytes());
            put(register("CanUploadToCloudClipboard"), &0u32.to_le_bytes());
        }
    }

    /// In a single clipboard session: save what's there, then replace it with the meme.
    pub fn swap_in(content: &Content) -> Result<Snapshot, String> {
        open()?;
        unsafe {
            let saved = read_all();
            EmptyClipboard();
            let ok = match content {
                // PNG first: apps take the first format they understand.
                Content::Image { png, dib } => {
                    let png_ok = put(register("PNG"), png);
                    put(CF_DIB, dib) || png_ok
                }
                Content::File(path) => {
                    let ok = put(CF_HDROP, &hdrop(path));
                    // "This is a copy, not a cut", so apps never move the original.
                    put(register("Preferred DropEffect"), &1u32.to_le_bytes());
                    ok
                }
            };
            exclude_from_history();
            CloseClipboard();
            if !ok {
                restore(&saved);
                return Err("Couldn't put the meme on the clipboard.".into());
            }
            Ok(saved)
        }
    }

    pub fn restore(snapshot: &Snapshot) {
        if open().is_err() {
            return;
        }
        unsafe {
            EmptyClipboard();
            for (format, bytes) in &snapshot.0 {
                put(*format, bytes);
            }
            // Putting your old clipboard back shouldn't show up as a new history entry.
            exclude_from_history();
            CloseClipboard();
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{encode, win};

    #[test]
    fn encodes_png_and_bottom_up_bgra_dib() {
        let path = std::env::temp_dir().join(format!("memeity-insert-{}.png", std::process::id()));
        // 2x1: red, then half-transparent blue.
        image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 128]).unwrap().save(&path).unwrap();
        let (png, dib) = encode(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(png.starts_with(b"\x89PNG"), "PNG bytes passed through");
        assert_eq!(dib.len(), 40 + 2 * 4);
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40, "biSize");
        assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 32, "biBitCount");
        assert_eq!(&dib[40..48], &[0, 0, 255, 255, 255, 0, 0, 128], "BGRA order");
    }

    #[test]
    fn hdrop_layout() {
        let bytes = win::hdrop(std::path::Path::new(r"C:\m\a.gif"));
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20, "pFiles");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1, "fWide");
        let units: Vec<u16> = bytes[20..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units[..units.len() - 2]).unwrap(), r"C:\m\a.gif");
        assert_eq!(&units[units.len() - 2..], &[0, 0], "double-NUL terminated");
    }
}

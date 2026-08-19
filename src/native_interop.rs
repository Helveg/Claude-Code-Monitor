use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

// Window style constants
pub const WS_POPUP_STYLE: u32 = 0x80000000;
pub const WS_CHILD_STYLE: u32 = 0x40000000;
pub const WS_CLIPSIBLINGS_STYLE: u32 = 0x04000000;

// Win event constants
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// Timer IDs
pub const TIMER_POLL: usize = 1;
pub const TIMER_COUNTDOWN: usize = 2;
pub const TIMER_RESET_POLL: usize = 3;
pub const TIMER_UPDATE_CHECK: usize = 4;
pub const TIMER_LAYOUT_REFRESH: usize = 5;

// Custom messages
pub const WM_APP: u32 = 0x8000;
pub const WM_APP_USAGE_UPDATED: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 3;

/// Get the taskbar window handle
pub fn find_taskbar() -> Option<HWND> {
    unsafe {
        let class = wide_str("Shell_TrayWnd");
        match FindWindowW(PCWSTR::from_raw(class.as_ptr()), PCWSTR::null()) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Find a child window by class name
pub fn find_child_window(parent: HWND, class_name: &str) -> Option<HWND> {
    unsafe {
        let class = wide_str(class_name);
        match FindWindowExW(
            parent,
            HWND::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
        ) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Get taskbar position via SHAppBarMessage
pub fn get_taskbar_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: taskbar_hwnd,
            ..Default::default()
        };
        let result = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        if result == 0 {
            return None;
        }
        Some(abd.rc)
    }
}

/// Get the bounding rectangle of a window
pub fn get_window_rect_safe(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

/// Embed our window as a child of the taskbar
pub fn embed_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) {
    unsafe {
        // Preserve existing extended style, add tool window + no activate
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32,
        );

        // Change from popup to child
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP_STYLE) | WS_CHILD_STYLE | WS_CLIPSIBLINGS_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let _ = SetParent(hwnd, taskbar_hwnd);
    }
}

/// Move the window
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

/// Set up a WinEvent hook for tray location changes
pub fn set_tray_event_hook(
    thread_id: u32,
    callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32),
) -> Option<HWINEVENTHOOK> {
    unsafe {
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(callback),
            0,
            thread_id,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.is_invalid() {
            None
        } else {
            Some(hook)
        }
    }
}

/// Get the thread ID that owns a window
pub fn get_window_thread_id(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// Unhook a WinEvent hook
pub fn unhook_win_event(hook: HWINEVENTHOOK) {
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

/// Convert a Rust string to a null-terminated wide string
pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// COLORREF wrapper (RGB packed into u32)
pub fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Color helper
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    #[allow(dead_code)]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Self { r, g, b }
    }

    pub fn to_colorref(self) -> u32 {
        colorref(self.r, self.g, self.b)
    }
}

/// The system I-beam, made solid white instead of screen-inverting.
///
/// `IDC_IBEAM` is drawn entirely out of invert-the-screen pixels, which is why
/// it reads as black over the terminal's Claude grey: with the cursor on the
/// GPU's hardware plane there is nothing to invert against, so DWM renders
/// those pixels black — black on near-black. Nothing about it is
/// theme-dependent, so no setting fixes it.
///
/// Rather than draw a replacement — whose shape, size and hotspot would then
/// have to be kept in step with the user's cursor scheme and DPI by hand —
/// this takes the real cursor's mask and repaints it: every inverting pixel
/// becomes solid white, with a black outline traced around the result so it
/// still reads if it strays onto light chrome. Same glyph, same size, same
/// hotspot.
///
/// Windows keeps a separate cursor bitmap per DPI, so this does too: `dpi` is
/// the monitor the panel is currently on, and the source cursor is loaded at
/// that monitor's cursor size. Caching one handle for the whole process
/// instead leaves a cursor sized for the old monitor — and recoloured from the
/// wrong variant — the moment the window is dragged to a screen with different
/// scaling.
///
/// Handles are owned for the life of the process: destroying a cursor while
/// the shell has it set would leave it pointing at freed memory, and there are
/// at most a handful.
pub fn light_ibeam_cursor(dpi: u32) -> HCURSOR {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<u32, isize>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = map.get(&dpi) {
        return HCURSOR(*handle as *mut _);
    }
    let cursor = unsafe { solid_ibeam(dpi) };
    map.insert(dpi, cursor.0 as isize);
    cursor
}

/// A 1-bpp cursor mask is two stacked planes, AND on top and XOR below. The
/// pair encodes transparent (1,0), invert-screen (1,1), black (0,0) or
/// white (0,1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MaskPixel {
    Transparent,
    Invert,
    Black,
    White,
}

fn mask_bit(bits: &[u8], stride: usize, plane: usize, x: i32, y: i32) -> bool {
    bits[plane + y as usize * stride + (x / 8) as usize] & (0x80 >> (x % 8)) != 0
}

fn set_mask_bit(bits: &mut [u8], stride: usize, plane: usize, x: i32, y: i32, on: bool) {
    let byte = &mut bits[plane + y as usize * stride + (x / 8) as usize];
    let bit = 0x80 >> (x % 8);
    if on {
        *byte |= bit;
    } else {
        *byte &= !bit;
    }
}

/// Repaint a cursor mask's inverting pixels as solid white on a black
/// outline. `bits` holds both planes; `h` is the height of *one* of them.
/// Returns false — leaving `bits` untouched — when the glyph doesn't invert,
/// which means it is already drawn in real colours and needs nothing.
fn solidify_inverted_mask(w: i32, h: i32, stride: usize, bits: &mut [u8]) -> bool {
    let plane = stride * h as usize;
    let pixel_at = |bits: &[u8], x: i32, y: i32| -> MaskPixel {
        match (
            mask_bit(bits, stride, 0, x, y),
            mask_bit(bits, stride, plane, x, y),
        ) {
            (true, false) => MaskPixel::Transparent,
            (true, true) => MaskPixel::Invert,
            (false, false) => MaskPixel::Black,
            (false, true) => MaskPixel::White,
        }
    };

    let inverts: Vec<bool> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| pixel_at(bits, x, y) == MaskPixel::Invert)
        .collect();
    if !inverts.iter().any(|i| *i) {
        return false;
    }
    let is_invert = |x: i32, y: i32| -> bool {
        (0..w).contains(&x) && (0..h).contains(&y) && inverts[(y * w + x) as usize]
    };

    for y in 0..h {
        for x in 0..w {
            let outline = (-1..=1).any(|dy| (-1..=1).any(|dx| is_invert(x + dx, y + dy)));
            let target = if is_invert(x, y) {
                MaskPixel::White
            } else if outline {
                MaskPixel::Black
            } else {
                // Anything the stock cursor drew in real colours stays as it
                // was; only the surround is forced transparent.
                match pixel_at(bits, x, y) {
                    MaskPixel::Invert | MaskPixel::Transparent => MaskPixel::Transparent,
                    other => other,
                }
            };
            let (and, xor) = match target {
                MaskPixel::Transparent => (true, false),
                MaskPixel::Invert => (true, true),
                MaskPixel::Black => (false, false),
                MaskPixel::White => (false, true),
            };
            set_mask_bit(bits, stride, 0, x, y, and);
            set_mask_bit(bits, stride, plane, x, y, xor);
        }
    }
    true
}

/// Invert a 32-bpp cursor's colours, alpha and shape untouched.
///
/// The cursor scheme's I-beam is a black glyph with a white outline, so
/// inverting turns it into a white glyph with a black outline — visible on the
/// terminal, still visible on light chrome.
///
/// Alpha has to survive the round trip, and `GetDIBits` with `BI_RGB` silently
/// zeroes it; a V5 header with explicit channel masks is what keeps it. Losing
/// it means treating the transparent surround as opaque black, which inverts
/// into an opaque white block — a 64x64 white square dragged around the
/// screen.
///
/// `None` when the cursor isn't in a shape we understand — the caller then
/// keeps the user's own cursor rather than guessing.
unsafe fn invert_colour_cursor(info: &ICONINFO) -> Option<HCURSOR> {
    use windows::Win32::Graphics::Gdi::*;

    let mut bm = BITMAP::default();
    if GetObjectW(
        info.hbmColor,
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut _),
    ) == 0
        || bm.bmBitsPixel != 32
    {
        crate::diagnose::log(format!("ibeam: colour cursor is {} bpp", bm.bmBitsPixel));
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);

    // BI_BITFIELDS + a V5 header names the alpha channel explicitly, which is
    // the only way GetDIBits hands it back. Negative height keeps the buffer
    // top-down so it can go straight back out again.
    let mut header = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: w,
        bV5Height: -h,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        ..Default::default()
    };
    let mut px = vec![0u32; (w * h) as usize];
    let screen = GetDC(HWND::default());
    let rows = GetDIBits(
        screen,
        info.hbmColor,
        0,
        h as u32,
        Some(px.as_mut_ptr() as *mut _),
        &mut header as *mut _ as *mut BITMAPINFO,
        DIB_RGB_COLORS,
    );
    ReleaseDC(HWND::default(), screen);
    if rows == 0 {
        crate::diagnose::log("ibeam: GetDIBits failed");
        return None;
    }

    // A 32-bpp cursor encodes transparency one of two ways, and they need
    // opposite treatment:
    //
    // * With an alpha channel it is an ordinary blended image — invert the
    //   colours and the glyph flips from dark to light.
    // * With no alpha the colour bitmap *is* the XOR plane, paired with the
    //   1-bpp AND mask, exactly like a monochrome cursor. Where the mask says
    //   "leave the screen alone" but the colour is non-black, the cursor XORs
    //   the screen — and a cursor on the GPU's hardware plane has nothing to
    //   XOR against, so those pixels come out black. Inverting colours there
    //   changes nothing; the pixels have to stop XOR-ing and become opaque.
    let alpha_pixels = px.iter().filter(|p| *p >> 24 != 0).count();
    crate::diagnose::log(format!(
        "ibeam: colour cursor {w}x{h} rows={rows} alpha_px={alpha_pixels}"
    ));
    dump_cursor_bitmap("ibeam-before", w, h, &px);

    let mut replacement_mask: Option<Vec<u8>> = None;
    if alpha_pixels > 0 {
        for p in px.iter_mut() {
            *p = invert_premultiplied(*p);
        }
    } else {
        let (mut mask_bits, stride) = read_mask_plane(info, w, h)?;
        if !solidify_xor_colour(w, h, &mut px, &mut mask_bits, stride) {
            crate::diagnose::log("ibeam: nothing xors the screen, left alone");
            return None;
        }
        replacement_mask = Some(mask_bits);
    }
    dump_cursor_bitmap("ibeam-after", w, h, &px);

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(
        HDC::default(),
        &header as *const _ as *const BITMAPINFO,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    )
    .ok()?;
    if bits.is_null() {
        let _ = DeleteObject(color);
        return None;
    }
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());

    // Turning XOR pixels opaque means the AND mask changed too; the alpha
    // path leaves it alone and reuses the original.
    let mask = match &replacement_mask {
        Some(bits) => {
            let created = CreateBitmap(w, h, 1, 1, Some(bits.as_ptr() as *const _));
            if created.is_invalid() {
                let _ = DeleteObject(color);
                return None;
            }
            created
        }
        None => info.hbmMask,
    };
    let mut recoloured = ICONINFO {
        fIcon: false.into(),
        xHotspot: info.xHotspot,
        yHotspot: info.yHotspot,
        // CreateIconIndirect copies both bitmaps, so handing it the mask we
        // were given is fine — the caller still owns and frees it.
        hbmMask: mask,
        hbmColor: color,
    };
    let cursor = CreateIconIndirect(&mut recoloured);
    let _ = DeleteObject(color);
    if replacement_mask.is_some() {
        let _ = DeleteObject(mask);
    }
    match cursor {
        Ok(icon) => {
            crate::diagnose::log("ibeam: built solid colour cursor");
            Some(HCURSOR(icon.0))
        }
        Err(err) => {
            crate::diagnose::log(format!("ibeam: CreateIconIndirect failed: {err}"));
            None
        }
    }
}

/// A colour cursor's 1-bpp AND mask, with its row stride. Sized `w x h` —
/// unlike a monochrome cursor's mask, there is no second plane, because the
/// colour bitmap is the XOR plane.
unsafe fn read_mask_plane(info: &ICONINFO, w: i32, h: i32) -> Option<(Vec<u8>, usize)> {
    use windows::Win32::Graphics::Gdi::*;

    let mut bm = BITMAP::default();
    if GetObjectW(
        info.hbmMask,
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut _),
    ) == 0
        || bm.bmWidth != w
        || bm.bmHeight < h
    {
        crate::diagnose::log("ibeam: mask does not match the colour bitmap");
        return None;
    }
    let stride = bm.bmWidthBytes as usize;
    let mut bits = vec![0u8; stride * bm.bmHeight as usize];
    if GetBitmapBits(info.hbmMask, bits.len() as i32, bits.as_mut_ptr() as *mut _) == 0 {
        crate::diagnose::log("ibeam: mask read failed");
        return None;
    }
    bits.truncate(stride * h as usize);
    Some((bits, stride))
}

/// Turn a screen-inverting colour cursor into an opaque one.
///
/// A pixel inverts when the AND mask says "keep the screen" and the colour is
/// non-black — the pair is a XOR instruction, and on the hardware cursor plane
/// it renders as black. Every such pixel is cleared in the mask so it draws its
/// own colour instead, and the pixels around the glyph become an opaque black
/// outline so it reads on a light background too.
///
/// Returns false, leaving both buffers untouched, when nothing inverts: that
/// cursor is already drawn in real colours and needs no help.
fn solidify_xor_colour(w: i32, h: i32, px: &mut [u32], mask: &mut [u8], stride: usize) -> bool {
    let inverts: Vec<bool> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| {
            let colour = px[(y * w + x) as usize] & 0x00FF_FFFF;
            mask_bit(mask, stride, 0, x, y) && colour != 0
        })
        .collect();
    if !inverts.iter().any(|i| *i) {
        return false;
    }
    let inverts_at = |x: i32, y: i32| -> bool {
        (0..w).contains(&x) && (0..h).contains(&y) && inverts[(y * w + x) as usize]
    };

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            if inverts_at(x, y) {
                // Draw the colour it was going to XOR with, opaquely.
                set_mask_bit(mask, stride, 0, x, y, false);
                continue;
            }
            // Already-opaque pixels keep whatever they were.
            if !mask_bit(mask, stride, 0, x, y) {
                continue;
            }
            let touches_glyph = (-1..=1).any(|dy| (-1..=1).any(|dx| inverts_at(x + dx, y + dy)));
            if touches_glyph {
                px[i] = px[i] & 0xFF00_0000;
                set_mask_bit(mask, stride, 0, x, y, false);
            }
        }
    }
    true
}

/// Invert one premultiplied BGRA pixel, keeping its alpha.
///
/// Premultiplied means the colour has already been scaled by alpha, so the
/// inversion has to happen on the unscaled value and be scaled back — invert
/// the stored bytes directly and a half-transparent grey comes out brighter
/// than an opaque one. Fully transparent pixels stay exactly as they are.
fn invert_premultiplied(px: u32) -> u32 {
    let a = (px >> 24) & 0xFF;
    if a == 0 {
        return 0;
    }
    let channel = |shift: u32| -> u32 {
        let stored = (px >> shift) & 0xFF;
        // Undo the premultiply, invert, redo it. Rounded, so an opaque
        // channel round-trips exactly.
        let straight = (stored * 255 + a / 2) / a;
        let inverted = 255u32.saturating_sub(straight.min(255));
        (inverted * a + 127) / 255
    };
    (a << 24) | (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// Write a cursor bitmap out as a BMP next to the diagnostic log, so the
/// recolour can be inspected without hovering the live panel. No-op unless
/// `--diagnose` is on.
fn dump_cursor_bitmap(name: &str, w: i32, h: i32, px: &[u32]) {
    if !crate::diagnose::is_enabled() {
        return;
    }
    let path = std::env::temp_dir().join(format!("{name}.bmp"));
    let stride = (w * 4) as u32;
    let pixel_bytes = stride * h as u32;
    let mut out: Vec<u8> = Vec::with_capacity(122 + pixel_bytes as usize);
    // BITMAPFILEHEADER + BITMAPV4HEADER, which carries the alpha mask so
    // viewers show transparency rather than garbage.
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(122 + pixel_bytes).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&122u32.to_le_bytes());
    out.extend_from_slice(&108u32.to_le_bytes()); // V4 header size
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&(-h).to_le_bytes()); // top-down
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    out.extend_from_slice(&pixel_bytes.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
    out.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
    out.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
    out.extend_from_slice(&0xFF00_0000u32.to_le_bytes());
    out.extend_from_slice(b"BGRs");
    out.extend_from_slice(&[0u8; 48]);
    for p in px {
        out.extend_from_slice(&p.to_le_bytes());
    }
    let _ = std::fs::write(&path, out);
    crate::diagnose::log(format!("ibeam: wrote {}", path.display()));
}

unsafe fn solid_ibeam(dpi: u32) -> HCURSOR {
    use windows::Win32::Foundation::{HANDLE, HINSTANCE};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;

    // Size comes from the system, never from us: `SM_CXCURSOR` at this
    // monitor's DPI already folds in both the monitor's scaling and the user's
    // pointer-size setting. `CopyImage` with `LR_COPYFROMRESOURCE` re-reads the
    // cursor resource and renders the variant closest to that size, which is
    // what keeps the glyph crisp — scaling a finished bitmap instead is what
    // produces a stretched or shrunken pointer.
    let (cx, cy) = (
        GetSystemMetricsForDpi(SM_CXCURSOR, dpi),
        GetSystemMetricsForDpi(SM_CYCURSOR, dpi),
    );
    let shared = LoadCursorW(HINSTANCE::default(), IDC_IBEAM).unwrap_or_default();
    let stock = match CopyImage(
        HANDLE(shared.0),
        IMAGE_CURSOR,
        cx,
        cy,
        LR_COPYFROMRESOURCE,
    ) {
        Ok(sized) if !sized.is_invalid() => HCURSOR(sized.0),
        _ => shared,
    };
    crate::diagnose::log(format!("ibeam: source cursor for dpi={dpi} at {cx}x{cy}"));

    let mut info = ICONINFO::default();
    if GetIconInfo(HICON(stock.0), &mut info).is_err() {
        crate::diagnose::log("ibeam: GetIconInfo failed");
        return stock;
    }
    let cleanup = |info: &ICONINFO| {
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask);
        }
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
    };
    // Which of the two shapes a cursor takes depends on the process: a
    // per-monitor-DPI-aware one (this app) is handed the scheme's 32-bpp
    // colour cursor, while a DPI-unaware process gets the legacy 1-bpp
    // inverting one. Both need the same treatment, by different means.
    if !info.hbmColor.is_invalid() {
        let recoloured = invert_colour_cursor(&info);
        cleanup(&info);
        return recoloured.unwrap_or(stock);
    }

    let mut bm = BITMAP::default();
    let read = GetObjectW(
        info.hbmMask,
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut _),
    );
    // The mask holds both planes stacked, so its height must be even.
    if read == 0 || bm.bmHeight % 2 != 0 {
        crate::diagnose::log(format!("ibeam: bad mask read={read} h={}", bm.bmHeight));
        cleanup(&info);
        return stock;
    }

    let stride = bm.bmWidthBytes as usize;
    let half_h = bm.bmHeight / 2;
    let mut bits = vec![0u8; stride * bm.bmHeight as usize];
    if GetBitmapBits(info.hbmMask, bits.len() as i32, bits.as_mut_ptr() as *mut _) == 0 {
        crate::diagnose::log("ibeam: GetBitmapBits failed");
        cleanup(&info);
        return stock;
    }
    crate::diagnose::log(format!("ibeam: mask {}x{} stride={stride}", bm.bmWidth, bm.bmHeight));
    if !solidify_inverted_mask(bm.bmWidth, half_h, stride, &mut bits) {
        // Already a real-coloured glyph — a custom scheme, most likely. Leave
        // the user's cursor alone.
        crate::diagnose::log("ibeam: mask has no inverting pixels, left alone");
        cleanup(&info);
        return stock;
    }

    let mask = CreateBitmap(bm.bmWidth, bm.bmHeight, 1, 1, Some(bits.as_ptr() as *const _));
    if mask.is_invalid() {
        crate::diagnose::log("ibeam: CreateBitmap failed");
        cleanup(&info);
        return stock;
    }
    let mut recolored = ICONINFO {
        fIcon: false.into(),
        xHotspot: info.xHotspot,
        yHotspot: info.yHotspot,
        hbmMask: mask,
        hbmColor: HBITMAP::default(),
    };
    let cursor = CreateIconIndirect(&mut recolored);
    let _ = DeleteObject(mask);
    cleanup(&info);
    match cursor {
        Ok(icon) => {
            crate::diagnose::log(format!("ibeam: built solid cursor {:?}", icon.0));
            HCURSOR(icon.0)
        }
        Err(err) => {
            crate::diagnose::log(format!("ibeam: CreateIconIndirect failed: {err}"));
            stock
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out a tiny mask: `~` invert, `.` black, `#` white, space clear.
    fn mask_from(rows: &[&str]) -> (i32, i32, usize, Vec<u8>) {
        let h = rows.len() as i32;
        let w = rows[0].len() as i32;
        let stride = ((w + 15) / 16 * 2) as usize;
        let mut bits = vec![0u8; stride * (h * 2) as usize];
        let plane = stride * h as usize;
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                let (and, xor) = match ch {
                    '~' => (true, true),
                    '.' => (false, false),
                    '#' => (false, true),
                    _ => (true, false),
                };
                set_mask_bit(&mut bits, stride, 0, x as i32, y as i32, and);
                set_mask_bit(&mut bits, stride, plane, x as i32, y as i32, xor);
            }
        }
        (w, h, stride, bits)
    }

    fn render(w: i32, h: i32, stride: usize, bits: &[u8]) -> Vec<String> {
        let plane = stride * h as usize;
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        match (
                            mask_bit(bits, stride, 0, x, y),
                            mask_bit(bits, stride, plane, x, y),
                        ) {
                            (true, false) => ' ',
                            (true, true) => '~',
                            (false, false) => '.',
                            (false, true) => '#',
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// The fix in one picture: inverting pixels become solid white, and pick
    /// up a black outline so the glyph survives a light background too.
    #[test]
    fn inverting_pixels_become_white_on_a_black_outline() {
        let (w, h, stride, mut bits) = mask_from(&[
            "     ",
            "  ~  ",
            "  ~  ",
            "     ",
        ]);
        assert!(solidify_inverted_mask(w, h, stride, &mut bits));
        assert_eq!(
            render(w, h, stride, &bits),
            vec![" ... ", " .#. ", " .#. ", " ... "],
        );
    }

    /// A cursor scheme that already draws in real colours needs no help, and
    /// must come back untouched rather than blanked.
    #[test]
    fn a_solid_glyph_is_left_alone() {
        let (w, h, stride, mut bits) = mask_from(&[
            "     ",
            " .#. ",
            "     ",
        ]);
        let before = bits.clone();
        assert!(!solidify_inverted_mask(w, h, stride, &mut bits));
        assert_eq!(bits, before);
    }

    /// The encoding a real cursor scheme ships: no alpha, mask all ones, glyph
    /// drawn white in the colour plane — i.e. "XOR the screen with white".
    /// That is what renders black on the hardware cursor plane, so the glyph
    /// must end up opaque, with a black outline around it.
    #[test]
    fn an_xor_glyph_becomes_opaque_with_an_outline() {
        let (w, h) = (5i32, 4i32);
        let stride = ((w + 15) / 16 * 2) as usize;
        // Mask all ones: "leave the screen alone" everywhere.
        let mut mask = vec![0xFFu8; stride * h as usize];
        // Colour plane: a two-pixel white stem, black (i.e. nothing) elsewhere.
        let mut px = vec![0u32; (w * h) as usize];
        px[(1 * w + 2) as usize] = 0x00FF_FFFF;
        px[(2 * w + 2) as usize] = 0x00FF_FFFF;

        assert!(solidify_xor_colour(w, h, &mut px, &mut mask, stride));

        let drawn = |x: i32, y: i32| !mask_bit(&mask, stride, 0, x, y);
        let colour = |x: i32, y: i32| px[(y * w + x) as usize] & 0x00FF_FFFF;

        // The glyph draws its own white now instead of inverting.
        assert!(drawn(2, 1) && colour(2, 1) == 0x00FF_FFFF);
        assert!(drawn(2, 2) && colour(2, 2) == 0x00FF_FFFF);
        // Its neighbours became an opaque black outline.
        assert!(drawn(1, 1) && colour(1, 1) == 0);
        assert!(drawn(3, 2) && colour(3, 2) == 0);
        assert!(drawn(2, 0) && colour(2, 0) == 0);
        // Anything further away stays transparent.
        assert!(!drawn(0, 0));
        assert!(!drawn(4, 3));
    }

    /// A cursor already drawn in real colours must be left exactly as it is —
    /// the transform is only for the ones that would render black.
    #[test]
    fn a_non_xor_colour_cursor_is_left_alone() {
        let (w, h) = (4i32, 2i32);
        let stride = ((w + 15) / 16 * 2) as usize;
        // Mask zeroed: every pixel already opaque, nothing XORs.
        let mut mask = vec![0x00u8; stride * h as usize];
        let mut px = vec![0x00AB_CDEFu32; (w * h) as usize];
        let (before_px, before_mask) = (px.clone(), mask.clone());

        assert!(!solidify_xor_colour(w, h, &mut px, &mut mask, stride));
        assert_eq!(px, before_px);
        assert_eq!(mask, before_mask);
    }

    /// Inverting a premultiplied pixel: opaque black becomes white, and the
    /// alpha channel survives untouched. Getting this wrong is what turned the
    /// transparent surround into a white block.
    #[test]
    fn premultiplied_inversion_keeps_alpha() {
        // Opaque black -> opaque white, and back again.
        assert_eq!(invert_premultiplied(0xFF00_0000), 0xFFFF_FFFF);
        assert_eq!(invert_premultiplied(0xFFFF_FFFF), 0xFF00_0000);
        // Fully transparent stays exactly as it was: nothing to draw.
        assert_eq!(invert_premultiplied(0x0000_0000), 0);
        assert_eq!(invert_premultiplied(0x00FF_FFFF), 0);
    }

    /// A half-transparent black edge pixel is stored as premultiplied zero;
    /// inverted it must come back as half-transparent *white*, i.e. stored at
    /// half intensity — not full white, which would fringe the glyph.
    #[test]
    fn premultiplied_inversion_rescales_translucent_pixels() {
        let inverted = invert_premultiplied(0x8000_0000);
        assert_eq!(inverted >> 24, 0x80, "alpha must not change");
        let channel = inverted & 0xFF;
        assert!(
            (0x7E..=0x82).contains(&channel),
            "expected ~0x80, got {channel:#04x}",
        );
    }
}

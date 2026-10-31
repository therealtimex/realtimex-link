//! Window screenshots on Windows: `PrintWindow(PW_RENDERFULLCONTENT)` renders the window's own
//! content, even when other windows cover it (verified in the #2453 spike). JPEG encoding and
//! the change-detection thumbnail come from the shared core.
//!
//! Every screenshot is also read with Windows.Media.Ocr (on-device, the user's languages) so
//! `core::window_text` can tell when the user presents; then the display is shot instead.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

use crate::image::{encode_jpeg_bgra, thumbnail_bgra, Bgra};
use crate::text::TextLine;
use crate::window::WindowInfo;
use anyhow::{bail, Result};
use windows::core::BOOL;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors,
    GetDC, GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetWindowDpiAwarenessContext, SetProcessDpiAwarenessContext,
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetSystemMetrics, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, MONITORINFOF_PRIMARY,
    SM_CXSCREEN, SM_CYSCREEN,
};

use crate::procs;
use crate::{DisplayInfo, DisplayNotFound, Shot};

/// PW_RENDERFULLCONTENT: include DirectComposition/GPU content (browsers, Electron apps).
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);

/// Physical-pixel coordinates everywhere (window rects, bitmaps). Safe to call repeatedly.
pub fn init() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn hwnd_of(id: u32) -> HWND {
    HWND(id as usize as *mut c_void)
}

fn title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// The desktop itself (wallpaper and icons) is a titled top-level window too.
fn is_desktop(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    class == "Progman" || class == "WorkerW"
}

fn cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut value as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        )
        .is_ok()
            && value != 0
    }
}

/// The visible frame (without the invisible resize borders), or the window rect.
fn frame(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
        .is_ok()
    };
    if ok || unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
        Some(r)
    } else {
        None
    }
}

/// The window rect in the coordinates the window itself renders in. A window that is not
/// per-monitor DPI aware renders at its own DPI and DWM stretches it, so `PrintWindow` returns
/// the unstretched size: smaller than the physical rect on a scaled display.
fn render_rect(hwnd: HWND) -> Result<RECT> {
    let mut r = RECT::default();
    unsafe {
        let previous = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd));
        let result = GetWindowRect(hwnd, &mut r);
        if !previous.is_invalid() {
            SetThreadDpiAwarenessContext(previous);
        }
        result?;
    }
    Ok(r)
}

unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
    unsafe { (*(list.0 as *mut Vec<HWND>)).push(hwnd) };
    true.into()
}

/// Visible, uncloaked top-level windows with a title, big enough to matter.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    init();
    let mut hwnds: Vec<HWND> = vec![];
    unsafe { EnumWindows(Some(collect), LPARAM(&mut hwnds as *mut Vec<HWND> as isize))? };
    let processes = procs::snapshot()?;
    let mut out = vec![];
    for hwnd in hwnds {
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() || cloaked(hwnd) || is_desktop(hwnd) {
            continue;
        }
        let name = title(hwnd);
        let Some(r) = frame(hwnd) else { continue };
        let (w, h) = ((r.right - r.left) as f64, (r.bottom - r.top) as f64);
        if name.is_empty() || w < 200.0 || h < 150.0 {
            continue;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let root = procs::root_of(pid, &processes);
        let exe = processes
            .get(&root)
            .map(|p| p.exe.clone())
            .unwrap_or_default();
        out.push(WindowInfo {
            id: hwnd.0 as usize as u32,
            pid: pid as i32,
            app: exe.to_ascii_lowercase(),
            app_name: procs::display_name(&exe),
            title: name,
            on_screen: !unsafe { IsIconic(hwnd) }.as_bool(),
            frame: [r.left as f64, r.top as f64, w, h],
            floating: false,
            layer: None,
            unselectable_reason: None,
        });
    }
    Ok(out)
}

/// Screenshot one window, at most `max_width` pixels wide.
pub fn capture_window(id: u32, max_width: u32) -> Result<Shot> {
    init();
    let hwnd = hwnd_of(id);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        bail!("window {id} is gone");
    }
    if unsafe { IsIconic(hwnd) }.as_bool() {
        bail!("window {id} is minimized");
    }
    // Physical pixels: the window rect and the visible frame inside it (without the invisible
    // resize borders). PrintWindow draws the whole window rect at the window's own DPI.
    let mut wr = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut wr)? };
    let fr = frame(hwnd).unwrap_or(wr);
    let rr = render_rect(hwnd)?;
    let (w, h) = (rr.right - rr.left, rr.bottom - rr.top);
    let (pw, ph) = (wr.right - wr.left, wr.bottom - wr.top);
    if w <= 0 || h <= 0 || pw <= 0 || ph <= 0 {
        bail!("window {id} has no size");
    }
    // Crop to the frame, scaled from physical pixels to the rendered size.
    let sx = |v: i32| (v as i64 * w as i64 / pw as i64) as i32;
    let sy = |v: i32| (v as i64 * h as i64 / ph as i64) as i32;
    let (cx, cy) = (
        sx(fr.left - wr.left).clamp(0, w - 1),
        sy(fr.top - wr.top).clamp(0, h - 1),
    );
    let (cw, ch) = (
        sx(fr.right - fr.left).clamp(1, w - cx),
        sy(fr.bottom - fr.top).clamp(1, h - cy),
    );

    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let result = (|| -> Result<Shot> {
            let dib = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(mem, dib.into());
            let printed = PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT).as_bool();
            let shot = if printed && !bits.is_null() {
                let stride = w as usize * 4;
                let data = std::slice::from_raw_parts(bits as *const u8, stride * h as usize);
                let offset = cy as usize * stride + cx as usize * 4;
                let img = Bgra {
                    width: cw as usize,
                    height: ch as usize,
                    stride,
                    data: &data[offset..],
                };
                shot_from(&img, max_width, title(hwnd))
            } else {
                Err(anyhow::anyhow!("PrintWindow failed for window {id}"))
            };
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            shot
        })();
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result
    }
}

fn shot_from(img: &Bgra, max_width: u32, title: String) -> Result<Shot> {
    let (jpeg, width, height) = encode_jpeg_bgra(img, max_width as usize)?;
    Ok(Shot {
        jpeg,
        thumb: thumbnail_bgra(img),
        width: width as u32,
        height: height as u32,
        title,
        text: recognize_text(img),
    })
}

/// Screenshot the primary monitor (what a presenting user most often shares).
pub fn capture_display(max_width: u32) -> Result<Shot> {
    capture_display_on(None, max_width)
}

/// Screenshot `display` (a GDI device name from [`displays`]; None: the primary monitor). A
/// monitor that isn't attached is [`DisplayNotFound`], never another one.
pub fn capture_display_on(display: Option<&str>, max_width: u32) -> Result<Shot> {
    let area = monitor_area(display)?;
    with_display(area, |img| shot_from(img, max_width, "display".into()))
}

/// The same picture as [`capture_display`], as a live-view video frame: from Desktop Duplication
/// (a frame only when the desktop changed, #2685), else a GDI copy.
pub fn capture_display_frame(max_width: u32) -> Result<crate::Frame> {
    capture_display_frame_on(None, max_width)
}

/// The same picture as [`capture_display_on`], as a live-view video frame: from that monitor's
/// own duplication, else a GDI copy of its area.
pub fn capture_display_frame_on(display: Option<&str>, max_width: u32) -> Result<crate::Frame> {
    init();
    let area = monitor_area(display)?;
    if let Some(frame) = crate::win_dupl::display_frame(&area.device, max_width) {
        return Ok(frame);
    }
    with_display(area, |img| Ok(crate::Frame::scaled(img, max_width)))
}

/// A monitor: its GDI device name and its rectangle on the virtual desktop, in physical pixels.
pub(crate) struct Area {
    pub device: String,
    pub rect: RECT,
}

/// The area of `display`, or of the primary monitor for None. With no monitor listed (a session
/// without one), None is the screen at (0, 0) as before.
fn monitor_area(display: Option<&str>) -> Result<Area> {
    let monitors = monitors();
    let found = match display {
        Some(id) => monitors
            .into_iter()
            .find(|m| m.info.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow::Error::from(DisplayNotFound(id.to_string())))?,
        None => match monitors.into_iter().find(|m| m.info.primary) {
            Some(primary) => primary,
            None => {
                let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
                return Ok(Area {
                    device: String::new(),
                    rect: RECT { left: 0, top: 0, right: w, bottom: h },
                });
            }
        },
    };
    Ok(Area {
        device: found.info.id,
        rect: found.rect,
    })
}

/// Call `use_it` with the pixels of `area`, copied from the screen (GDI). Negative origins are
/// valid: the screen DC spans the whole virtual desktop.
fn with_display<T>(area: Area, use_it: impl FnOnce(&Bgra) -> Result<T>) -> Result<T> {
    init();
    let (x, y) = (area.rect.left, area.rect.top);
    let (w, h) = (area.rect.right - area.rect.left, area.rect.bottom - area.rect.top);
    if w <= 0 || h <= 0 {
        bail!("no display size");
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let result = (|| -> Result<T> {
            let dib = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(mem, dib.into());
            let copied = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY);
            let result = match copied {
                Ok(()) if !bits.is_null() => {
                    let data =
                        std::slice::from_raw_parts(bits as *const u8, w as usize * 4 * h as usize);
                    let img = Bgra {
                        width: w as usize,
                        height: h as usize,
                        stride: w as usize * 4,
                        data,
                    };
                    use_it(&img)
                }
                _ => Err(anyhow::anyhow!("copying the display failed")),
            };
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            result
        })();
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result
    }
}

/// One monitor as GDI lists it.
struct Monitor {
    info: DisplayInfo,
    rect: RECT,
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    list: LPARAM,
) -> BOOL {
    unsafe { (*(list.0 as *mut Vec<HMONITOR>)).push(monitor) };
    true.into()
}

/// The monitors on the desktop, in physical pixels (per-monitor DPI aware), named by
/// DisplayConfig. Empty when GDI lists none.
fn monitors() -> Vec<Monitor> {
    init();
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        );
    }
    let names = friendly_names();
    handles
        .into_iter()
        .filter_map(|handle| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if !unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
                .as_bool()
            {
                return None;
            }
            let device = wide_text(&info.szDevice);
            let rect = info.monitorInfo.rcMonitor;
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            if unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err() {
                dpi_x = 96;
            }
            Some(Monitor {
                info: DisplayInfo {
                    name: names.get(&device.to_ascii_uppercase()).cloned().unwrap_or_default(),
                    id: device,
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                    x: rect.left,
                    y: rect.top,
                    width: (rect.right - rect.left).max(0) as u32,
                    height: (rect.bottom - rect.top).max(0) as u32,
                    scale: dpi_x as f64 / 96.0,
                },
                rect,
            })
        })
        .collect()
}

/// The monitors, in the picker's order (realtimex-link#12).
pub fn displays() -> Result<Vec<DisplayInfo>> {
    let mut list: Vec<DisplayInfo> = monitors().into_iter().map(|m| m.info).collect();
    crate::sort_displays(&mut list);
    Ok(list)
}

/// UTF-16 up to its NUL.
fn wide_text(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The monitors' friendly names ("DELL U2720Q") by GDI device name (upper case), from
/// DisplayConfig's active paths; empty when it can't say.
fn friendly_names() -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    unsafe {
        let (mut path_count, mut mode_count) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).is_err() {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .is_err()
        {
            return names;
        }
        for path in &paths[..(path_count as usize).min(paths.len())] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                continue;
            }
            let name = wide_text(&target.monitorFriendlyDeviceName);
            if !name.is_empty() {
                names
                    .entry(wide_text(&source.viewGdiDeviceName).to_ascii_uppercase())
                    .or_insert(name);
            }
        }
    }
    names
}

/// Text in the image, recognized on-device (Windows.Media.Ocr, the user's languages); empty if
/// no OCR language is installed or recognition fails. Windows reports no confidence per line.
fn recognize_text(img: &Bgra) -> Vec<TextLine> {
    recognize(img).unwrap_or_default()
}

/// OCR is a WinRT call: this thread needs COM (the multithreaded apartment, as audio uses).
fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn recognize(img: &Bgra) -> windows::core::Result<Vec<TextLine>> {
    com_init();
    let (mut w, mut h) = (img.width, img.height);
    let mut bytes = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = row * img.stride;
        bytes.extend_from_slice(&img.data[start..start + w * 4]);
    }
    let max = OcrEngine::MaxImageDimension()? as usize;
    while w > max || h > max {
        (bytes, w, h) = halve(&bytes, w, h);
    }
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
    let buffer = CryptographicBuffer::CreateFromByteArray(&bytes)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        w as i32,
        h as i32,
    )?;
    let result = engine.RecognizeAsync(&bitmap)?.join()?;
    let mut lines = vec![];
    for line in result.Lines()? {
        let text = line.Text()?.to_string();
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, 0f32, 0f32);
        for word in line.Words()? {
            let r = word.BoundingRect()?;
            x0 = x0.min(r.X);
            y0 = y0.min(r.Y);
            x1 = x1.max(r.X + r.Width);
            y1 = y1.max(r.Y + r.Height);
        }
        if x1 > x0 && y1 > y0 {
            let (fw, fh) = (w as f32, h as f32);
            lines.push(TextLine::new(
                &text,
                1.0,
                [x0 / fw, y0 / fh, (x1 - x0) / fw, (y1 - y0) / fh],
            ));
        }
    }
    Ok(lines)
}

/// Half size (2x2 box filter), for images beyond the OCR engine's maximum dimension.
fn halve(bytes: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = (w / 2, h / 2);
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let p =
                    |dx: usize, dy: usize| bytes[((2 * y + dy) * w + 2 * x + dx) * 4 + c] as u32;
                out[(y * nw + x) * 4 + c] = ((p(0, 0) + p(1, 0) + p(0, 1) + p(1, 1)) / 4) as u8;
            }
        }
    }
    (out, nw, nh)
}

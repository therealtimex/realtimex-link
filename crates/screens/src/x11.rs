//! Window listing and screenshots on X11 (and of XWayland apps under Wayland), through x11rb.
//!
//! Windows are the window manager's client list (EWMH `_NET_CLIENT_LIST`). A screenshot reads
//! the window itself (GetImage). Under a compositor, which every current desktop runs, each
//! window has its own pixmap, so that is the window's content even where other windows cover it
//! (checked on GNOME 46 with a window on top). Without one it would be what is on screen.

use crate::image::{encode_jpeg_bgra, thumbnail_bgra, Bgra};
use crate::window::WindowInfo;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::{DisplayInfo, DisplayNotFound, Shot};

struct X {
    conn: RustConnection,
    root: Window,
}

/// Why screenshots can't work in this session, if they can't.
pub fn unavailable() -> Option<String> {
    X::connect().err().map(|e| format!("{e:#}"))
}

/// True in a Wayland session, where only XWayland apps' windows are visible to X11.
pub fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

impl X {
    fn connect() -> Result<Self> {
        if std::env::var_os("DISPLAY").is_none() {
            bail!("no X11 display (DISPLAY is not set)");
        }
        let (conn, screen) = x11rb::connect(None).context("connect to the X11 display")?;
        let root = conn.setup().roots[screen].root;
        Ok(Self { conn, root })
    }

    fn atom(&self, name: &str) -> Result<u32> {
        Ok(self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }

    fn property(&self, win: Window, prop: u32, kind: impl Into<u32>) -> Result<Vec<u8>> {
        Ok(self
            .conn
            .get_property(false, win, prop, kind, 0, 1 << 16)?
            .reply()?
            .value)
    }

    fn property32(&self, win: Window, prop: u32, kind: impl Into<u32>) -> Result<Vec<u32>> {
        Ok(self
            .conn
            .get_property(false, win, prop, kind, 0, 1 << 16)?
            .reply()?
            .value32()
            .map(|v| v.collect())
            .unwrap_or_default())
    }

    /// `width` x `height` pixels of `drawable` at (x, y), as BGRA rows.
    fn image(&self, drawable: u32, x: i16, y: i16, width: u16, height: u16) -> Result<Vec<u8>> {
        let setup = self.conn.setup();
        if setup.image_byte_order != ImageOrder::LSB_FIRST {
            bail!("unsupported X11 image byte order");
        }
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, x, y, width, height, !0)?
            .reply()?;
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == reply.depth)
            .map(|f| f.bits_per_pixel);
        if bpp != Some(32) {
            bail!("unsupported X11 pixel format (depth {})", reply.depth);
        }
        if reply.data.len() < width as usize * height as usize * 4 {
            bail!("short X11 image");
        }
        Ok(reply.data)
    }
}

/// The program behind a process: its executable's name, e.g. "chrome" or "firefox".
pub fn exe_name(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()?
        .file_name()?
        .to_str()
        .map(|s| s.trim_end_matches(" (deleted)").to_lowercase())
}

/// Atoms `list_windows` reads per window.
struct ListAtoms {
    net_pid: u32,
    net_state: u32,
    hidden: u32,
    window_type: u32,
    not_apps: [u32; 2],
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let x = X::connect()?;
    let client_list = x.atom("_NET_CLIENT_LIST")?;
    let atoms = ListAtoms {
        net_pid: x.atom("_NET_WM_PID")?,
        net_state: x.atom("_NET_WM_STATE")?,
        hidden: x.atom("_NET_WM_STATE_HIDDEN")?,
        window_type: x.atom("_NET_WM_WINDOW_TYPE")?,
        // The desktop's icons and panels are windows too.
        not_apps: [
            x.atom("_NET_WM_WINDOW_TYPE_DESKTOP")?,
            x.atom("_NET_WM_WINDOW_TYPE_DOCK")?,
        ],
    };
    let windows = x.property32(x.root, client_list, AtomEnum::WINDOW)?;
    if windows.is_empty() {
        bail!("the window manager lists no windows (_NET_CLIENT_LIST)");
    }
    // A window can close between the list and its own requests (BadWindow): skip it, rather
    // than fail the whole list (code review of !2349).
    Ok(windows
        .into_iter()
        .filter_map(|win| window_info(&x, win, &atoms).ok().flatten())
        .collect())
}

/// One client window, or None when it isn't one to list (no title, too small, a dock).
fn window_info(x: &X, win: Window, a: &ListAtoms) -> Result<Option<WindowInfo>> {
    let geo = x.conn.get_geometry(win)?.reply()?;
    let pos = x.conn.translate_coordinates(win, x.root, 0, 0)?.reply()?;
    let (w, h) = (geo.width as f64, geo.height as f64);
    let title = window_title(x, win);
    if title.is_empty() || w < 200.0 || h < 150.0 {
        return Ok(None);
    }
    let kinds = x.property32(win, a.window_type, AtomEnum::ATOM)?;
    if kinds.iter().any(|k| a.not_apps.contains(k)) {
        return Ok(None);
    }
    let pid = x
        .property32(win, a.net_pid, AtomEnum::CARDINAL)?
        .first()
        .map(|&p| p as i32)
        .unwrap_or(0);
    // WM_CLASS: instance and class, NUL-separated ("Google-chrome").
    let class = x.property(win, AtomEnum::WM_CLASS.into(), AtomEnum::STRING)?;
    let mut parts = class
        .split(|&b| b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts
        .next()
        .filter(|c| !c.is_empty())
        .unwrap_or(instance.clone());
    let viewable = x.conn.get_window_attributes(win)?.reply()?.map_state == MapState::VIEWABLE;
    let minimized = x
        .property32(win, a.net_state, AtomEnum::ATOM)?
        .contains(&a.hidden);
    Ok(Some(WindowInfo {
        id: win,
        pid,
        app: (pid > 0)
            .then(|| exe_name(pid))
            .flatten()
            .unwrap_or_else(|| instance.to_lowercase()),
        app_name: class.replace('-', " "),
        title,
        on_screen: viewable && !minimized,
        frame: [pos.dst_x as f64, pos.dst_y as f64, w, h],
        floating: false,
        layer: None,
        unselectable_reason: None,
    }))
}

pub fn shot(
    data: &[u8],
    width: usize,
    height: usize,
    max_width: u32,
    title: String,
) -> Result<Shot> {
    let img = Bgra {
        width,
        height,
        stride: width * 4,
        data,
    };
    let (jpeg, w, h) = encode_jpeg_bgra(&img, max_width as usize)?;
    Ok(Shot {
        jpeg,
        thumb: thumbnail_bgra(&img),
        width: w as u32,
        height: h as u32,
        title,
        text: vec![],
    })
}

/// Screenshot one window, at most `max_width` pixels wide.
pub fn capture_window(id: u32, max_width: u32) -> Result<Shot> {
    let x = X::connect()?;
    let attrs = x
        .conn
        .get_window_attributes(id)?
        .reply()
        .map_err(|_| anyhow!("window {id} is gone"))?;
    if attrs.map_state != MapState::VIEWABLE {
        bail!("window {id} is not on screen (minimized, or on another workspace)");
    }
    let geo = x.conn.get_geometry(id)?.reply()?;
    let (w, h) = (geo.width, geo.height);
    let title = window_title(&x, id);
    let data = x.image(id, 0, 0, w, h)?;
    shot(&data, w as usize, h as usize, max_width, title)
}

/// `_NET_WM_NAME` (UTF-8), else `WM_NAME`.
fn window_title(x: &X, id: Window) -> String {
    let utf8 = (|| x.property(id, x.atom("_NET_WM_NAME")?, x.atom("UTF8_STRING")?))();
    let bytes = match utf8 {
        Ok(v) if !v.is_empty() => v,
        _ => x
            .property(id, AtomEnum::WM_NAME.into(), AtomEnum::STRING)
            .unwrap_or_default(),
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The primary monitor (what a presenting user most often shares), else the whole screen.
pub fn capture_display(max_width: u32) -> Result<Shot> {
    capture_display_on(None, max_width)
}

/// Screenshot `display` (a RandR monitor name from [`displays`]; None: the primary monitor, else
/// the whole screen). A monitor that isn't there is [`DisplayNotFound`], never another one.
pub fn capture_display_on(display: Option<&str>, max_width: u32) -> Result<Shot> {
    let x = X::connect()?;
    let (rx, ry, rw, rh) = monitor_area(&x, display)?;
    let data = x.image(x.root, rx, ry, rw, rh)?;
    shot(&data, rw as usize, rh as usize, max_width, "display".into())
}

/// The same picture as [`capture_display`], as a live-view video frame (#2683): read through one
/// connection kept for the helper's life, through shared memory when the server offers it, and
/// scaled and encoded only when it differs from the last read. A new connection, a full GetImage
/// over the socket, a scale and an encode per frame cost about a core on the T30. (XDamage on the
/// root window is no help: under a compositor the windows draw offscreen and it reports nothing.)
pub fn capture_display_frame(max_width: u32) -> Result<crate::Frame> {
    capture_display_frame_on(None, max_width)
}

/// The same picture as [`capture_display_on`], as a live-view video frame: each monitor a live
/// view shows has its own reader on the shared connection (realtimex-link#12), dropped once no
/// view has asked for it in IDLE_DROP.
pub fn capture_display_frame_on(display: Option<&str>, max_width: u32) -> Result<crate::Frame> {
    let mut live = LIVE.lock().unwrap();
    if live.is_none() {
        *live = Some(Lives {
            x: X::connect()?,
            monitors: HashMap::new(),
        });
    }
    let lives = live.as_mut().unwrap();
    lives.drop_idle();
    let key = display.map(str::to_string);
    if !lives.monitors.contains_key(&key) {
        let reader = Live::open(&lives.x, display)?;
        lives.monitors.insert(key.clone(), reader);
    }
    let reader = lives.monitors.get_mut(&key).expect("just added");
    let result = reader.frame(&lives.x, max_width);
    match &result {
        Ok(_) => {}
        // That monitor is gone: only its reader goes.
        Err(error) if error.is::<DisplayNotFound>() => {
            if let Some(gone) = lives.monitors.remove(&key) {
                gone.release(&lives.x);
            }
        }
        // A display change or a lost connection: start over on the next frame.
        Err(_) => *live = None,
    }
    result
}

static LIVE: Mutex<Option<Lives>> = Mutex::new(None);

/// How often a live view re-reads the monitor area (one RandR request).
const GEOMETRY_CHECK: Duration = Duration::from_secs(2);
/// A monitor's reader no live view asked a frame of for this long is dropped.
const IDLE_DROP: Duration = Duration::from_secs(5);

/// The live readers on one connection, by the monitor asked for (None: the primary).
struct Lives {
    x: X,
    monitors: HashMap<Option<String>, Live>,
}

impl Lives {
    fn drop_idle(&mut self) {
        let idle: Vec<Option<String>> = self
            .monitors
            .iter()
            .filter(|(_, live)| live.last_asked.elapsed() >= IDLE_DROP)
            .map(|(key, _)| key.clone())
            .collect();
        for key in idle {
            if let Some(live) = self.monitors.remove(&key) {
                live.release(&self.x);
            }
        }
    }
}

impl Drop for Lives {
    fn drop(&mut self) {
        for (_, live) in self.monitors.drain() {
            live.release(&self.x);
        }
        let _ = self.x.conn.flush();
    }
}

struct Live {
    /// The monitor asked for (None: the primary).
    display: Option<String>,
    /// Its area on the root window, checked every GEOMETRY_CHECK: a resolution or
    /// primary-monitor change does not make GetImage fail.
    area: (i16, i16, u16, u16),
    area_checked: Instant,
    shm: Option<SharedImage>,
    /// The last screen read, unscaled, to tell whether the next one changed.
    last_read: Vec<u8>,
    /// The last frame and the width it was made for.
    last: Option<(crate::Frame, u32)>,
    last_asked: Instant,
}

impl Live {
    fn open(x: &X, display: Option<&str>) -> Result<Self> {
        let (rx, ry, rw, rh) = monitor_area(x, display)?;
        let shm = SharedImage::new(x, rw as usize * rh as usize * 4).ok();
        Ok(Self {
            display: display.map(str::to_string),
            area: (rx, ry, rw, rh),
            area_checked: Instant::now(),
            shm,
            last_read: Vec::new(),
            last: None,
            last_asked: Instant::now(),
        })
    }

    fn frame(&mut self, x: &X, max_width: u32) -> Result<crate::Frame> {
        self.last_asked = Instant::now();
        if self.area_checked.elapsed() >= GEOMETRY_CHECK {
            self.area_checked = Instant::now();
            let area = monitor_area(x, self.display.as_deref())?;
            if area != self.area {
                self.resize(x, area);
            }
        }
        let (rx, ry, width, height) = self.area;
        let owned;
        let data = match &self.shm {
            Some(shm) => shm.read(x, self.area)?,
            None => {
                owned = x.image(x.root, rx, ry, width, height)?;
                &owned[..]
            }
        };
        if let Some((frame, made_for)) = &self.last {
            if *made_for == max_width && self.last_read == data {
                return Ok(frame.clone());
            }
        }
        self.last_read.clear();
        self.last_read.extend_from_slice(data);
        let frame = crate::Frame {
            seq: crate::next_frame_seq(),
            ..crate::Frame::scaled(
                &Bgra {
                    width: width as usize,
                    height: height as usize,
                    stride: width as usize * 4,
                    data,
                },
                max_width,
            )
        };
        self.last = Some((frame.clone(), max_width));
        Ok(frame)
    }

    /// A new monitor area: a shared segment of the new size, and nothing cached from the old one.
    fn resize(&mut self, x: &X, area: (i16, i16, u16, u16)) {
        self.area = area;
        if let Some(shm) = self.shm.take() {
            shm.release(x);
        }
        self.shm = SharedImage::new(x, area.2 as usize * area.3 as usize * 4).ok();
        self.last_read.clear();
        self.last = None;
    }

    fn release(mut self, x: &X) {
        if let Some(shm) = self.shm.take() {
            shm.release(x);
        }
    }
}

/// A System V shared-memory segment attached to the X server (MIT-SHM): the server writes the
/// screen into it instead of sending it over the socket.
struct SharedImage {
    seg: u32,
    addr: *mut u8,
    len: usize,
}

// Only used under the LIVE lock.
unsafe impl Send for SharedImage {}

impl SharedImage {
    fn new(x: &X, len: usize) -> Result<Self> {
        x.conn.shm_query_version()?.reply()?;
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            bail!("shmget failed");
        }
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        // Marked for removal now: it goes away once both sides detach, even if we crash.
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        if addr as isize == -1 {
            bail!("shmat failed");
        }
        let seg = x.conn.generate_id()?;
        if let Err(e) = x
            .conn
            .shm_attach(seg, id as u32, false)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(anyhow::Error::from))
        {
            unsafe { libc::shmdt(addr) };
            return Err(e);
        }
        Ok(Self {
            seg,
            addr: addr.cast(),
            len,
        })
    }

    fn read(&self, x: &X, (rx, ry, w, h): (i16, i16, u16, u16)) -> Result<&[u8]> {
        let reply = x
            .conn
            .shm_get_image(
                x.root,
                rx,
                ry,
                w,
                h,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                self.seg,
                0,
            )?
            .reply()?;
        let size = w as usize * h as usize * 4;
        if reply.depth < 24 || (reply.size as usize) < size || size > self.len {
            bail!("unsupported X11 pixel format for shared memory");
        }
        Ok(unsafe { std::slice::from_raw_parts(self.addr, size) })
    }

    fn release(self, x: &X) {
        let _ = x.conn.shm_detach(self.seg);
        unsafe { libc::shmdt(self.addr.cast()) };
    }
}

/// The id of the whole screen when RandR lists no monitor.
const WHOLE_SCREEN: &str = "screen";

/// The RandR monitors (else the whole screen as one), in the picker's order (realtimex-link#12).
pub fn displays() -> Result<Vec<DisplayInfo>> {
    let x = X::connect()?;
    let mut list: Vec<DisplayInfo> = monitors(&x)?
        .into_iter()
        .map(|(id, area, primary)| DisplayInfo {
            name: id.clone(),
            id,
            primary,
            x: area.0 as i32,
            y: area.1 as i32,
            width: area.2 as u32,
            height: area.3 as u32,
            scale: 1.0,
        })
        .collect();
    crate::sort_displays(&mut list);
    Ok(list)
}

/// A monitor's name, area on the root window and whether it is the primary one.
type Monitor = (String, (i16, i16, u16, u16), bool);

/// Each monitor; the whole screen alone (named WHOLE_SCREEN) when RandR lists none.
fn monitors(x: &X) -> Result<Vec<Monitor>> {
    let mut list = Vec::new();
    if let Ok(reply) = x.conn.randr_get_monitors(x.root, true)?.reply() {
        for m in reply.monitors {
            let name = x
                .conn
                .get_atom_name(m.name)?
                .reply()
                .map(|n| String::from_utf8_lossy(&n.name).into_owned())
                .unwrap_or_default();
            list.push((name, (m.x, m.y, m.width, m.height), m.primary));
        }
    }
    if list.is_empty() {
        let root = x.conn.get_geometry(x.root)?.reply()?;
        list.push((WHOLE_SCREEN.to_string(), (0, 0, root.width, root.height), true));
    }
    Ok(list)
}

/// The area of `display` on the root window: the monitor named, or for None the primary one,
/// else the first, else the whole screen.
fn monitor_area(x: &X, display: Option<&str>) -> Result<(i16, i16, u16, u16)> {
    let Some(id) = display else {
        return primary_area(x);
    };
    monitors(x)?
        .into_iter()
        .find(|(name, _, _)| name == id)
        .map(|(_, area, _)| area)
        .ok_or_else(|| DisplayNotFound(id.to_string()).into())
}

/// The primary monitor's area on the root window, else the whole screen.
fn primary_area(x: &X) -> Result<(i16, i16, u16, u16)> {
    let root = x.conn.get_geometry(x.root)?.reply()?;
    let (mut rx, mut ry, mut rw, mut rh) = (0, 0, root.width, root.height);
    if let Ok(monitors) = x.conn.randr_get_monitors(x.root, true)?.reply() {
        if let Some(m) = monitors
            .monitors
            .iter()
            .find(|m| m.primary)
            .or(monitors.monitors.first())
        {
            (rx, ry, rw, rh) = (m.x, m.y, m.width, m.height);
        }
    }
    Ok((rx, ry, rw, rh))
}

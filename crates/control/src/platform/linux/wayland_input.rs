//! Remote Control on GNOME Wayland: Mutter's own RemoteDesktop and ScreenCast D-Bus APIs (the
//! ones gnome-remote-desktop uses), so no portal dialog. GNOME shows its screen-sharing
//! indicator while the session runs.
//!
//! Screenshots come from the session's PipeWire stream, read continuously by one
//! `gst-launch-1.0` (gstreamer1.0-tools + gstreamer1.0-pipewire) into a FIFO, keeping
//! rtx-control free of PipeWire linking, as rtx-capture's audio side does with PipeWire's CLI tools.
//! A screenshot is the latest frame, so a live view is not held to one new pipeline (about
//! half a second) per frame.
//!
//! A computer with no monitor attached (headless, #9) has no monitor to record: the session
//! records a virtual monitor instead (or `virtualDisplays` of them, realtimex-link#12), which
//! Mutter sizes from the stream's negotiated format, as gnome-remote-desktop does headless.
//!
//! Several displays (realtimex-link#12): the monitors come from Mutter's DisplayConfig
//! (`GetCurrentState`), each recorded as a stream of its own; a display's id is its connector
//! (`HDMI-1`), a virtual one's `Meta-n` in the order this session made them. Streams can only be
//! added before the session starts (a later one never starts), so every monitor is recorded
//! then; a recorded stream nobody reads costs nothing measurable (T30 spike, GNOME 50), and a
//! frame pipeline runs only for a display in use, stopping after STREAM_IDLE. Pointer moves go
//! to the shown display's stream, in its pixels. A monitor plugged in or out (DisplayConfig's
//! MonitorsChanged; a removed monitor's stream only freezes) starts the session again with the
//! new set, once the change has settled for RESTART_SETTLE.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

use super::super::{Button, Shot};
use super::terminal;
use crate::displays::{Display, DisplayInfo, DisplayNotFound, Rect};

const RD_BUS: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_BUS: &str = "org.gnome.Mutter.ScreenCast";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const CURSOR_EMBEDDED: u32 = 1;
/// The virtual monitor's size when no monitor is attached: Mutter makes it the size the stream
/// negotiates.
const VIRTUAL_SIZE: (u32, u32) = (1920, 1080);
/// A display's frame pipeline no request has used for this long stops; its stream stays
/// recorded (realtimex-link#12).
const STREAM_IDLE: Duration = Duration::from_secs(5);
const DC_BUS: &str = "org.gnome.Mutter.DisplayConfig";
/// A monitor change waits this long without another before the session starts again: plugging
/// a monitor in announces several changes.
const RESTART_SETTLE: Duration = Duration::from_secs(1);
/// Mutter's connectors for virtual monitors (Link's own, headless), and their display name.
const VIRTUAL_CONNECTOR: &str = "Meta-";
const VIRTUAL_VENDOR: &str = "MetaVendor";

/// How many virtual monitors a headless session records: Link's `virtualDisplays`, from
/// `start` (1 to 4; 1 as since 0.2.6). Taken when a session starts.
static VIRTUAL_DISPLAYS: AtomicU32 = AtomicU32::new(1);

pub fn set_virtual_displays(count: u32) {
    VIRTUAL_DISPLAYS.store(count.clamp(1, 4), Ordering::Relaxed);
}
const STREAM_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_TIMEOUT: Duration = Duration::from_secs(8);
/// How often a virtual monitor's first-frame wait checks the writer and nudges the pointer.
const READY_POLL: Duration = Duration::from_millis(250);
/// Frames read from the stream at most this often; Mutter sends one per repaint.
const MAX_FRAME_RATE: u32 = 15;
/// The frame pipe's buffer, asked for with F_SETPIPE_SZ.
const PIPE_SIZE: libc::c_int = 1 << 20;
/// A screenshot waits this long for a frame newer than the request before nudging the
/// pointer, and as long again after it, then settles for the latest frame.
const FRESH_FRAME_WAIT: Duration = Duration::from_millis(150);
/// A live view's frame waits at most this long after the nudge.
const LIVE_FRAME_WAIT: Duration = Duration::from_millis(50);
const TEXT_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
const KEYSYM_CTRL: u32 = 0xffe3;
/// How long the clipboard's owner has to write its data (#2682).
const SELECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The strings of an `as` D-Bus value (GNOME's mime-types), whatever form zbus hands it in.
fn mime_list(value: &OwnedValue) -> Vec<String> {
    fn strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Str(text) => out.push(text.to_string()),
            Value::Value(inner) => strings(inner, out),
            // GNOME sends the list wrapped in a structure: (as).
            Value::Structure(structure) => structure
                .fields()
                .iter()
                .for_each(|field| strings(field, out)),
            Value::Array(array) => array.iter().for_each(|item| strings(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    strings(value, &mut out);
    out
}

/// The clipboard as GNOME last announced it.
#[derive(Default)]
struct Selection {
    changes: u64,
    mime_types: Vec<String>,
    own: bool,
}
const KEYSYM_SHIFT: u32 = 0xffe1;
const KEYSYM_V: u32 = 0x76;
/// Time for the focused app to fetch the selection we just announced, and to paste it.
const PASTE_SETTLE: Duration = Duration::from_millis(150);

/// Linux evdev button codes.
fn evdev_button(button: Button) -> i32 {
    match button {
        Button::Left => 0x110,
        Button::Right => 0x111,
        Button::Middle => 0x112,
    }
}

pub struct WaylandInput {
    session: Proxy<'static>,
    /// The displays this session records, the primary first.
    recorded: Vec<Recorded>,
    /// The session records virtual monitors (none attached).
    headless: bool,
    /// The physical connectors this session was made for, and when DisplayConfig last said the
    /// monitors changed (written by the MonitorsChanged watch).
    connectors: Vec<String>,
    monitors_changed: Arc<Mutex<Option<Instant>>>,
    /// When the monitors' places and primary flag were last read (DisplayConfig).
    placed_at: Option<Instant>,
    /// What the clipboard serves, per MIME type, as last announced.
    clipboard_data: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    clipboard_enabled: bool,
    /// The clipboard as GNOME last announced it (#2682): changes, the offered MIME types, and
    /// whether this session owns it (its own paste).
    selection: Arc<Mutex<Selection>>,
}

/// One display the session records: a monitor, or a virtual one.
struct Recorded {
    /// As listed: the connector or `Meta-n` (by creation), and the monitor's place and size.
    info: DisplayInfo,
    stream_path: OwnedObjectPath,
    node_id: u32,
    /// The virtual monitor's size, when it is one.
    virtual_size: Option<(u32, u32)>,
    frames: Option<FrameStream>,
    /// The stream's frame size: pointer coordinates are in these pixels, not the logical
    /// monitor size Mutter reports (they differ under fractional scaling).
    frame_size: Option<(u32, u32)>,
    /// The last live-view frame: the stream frame it came from, the width it was made for.
    last_frame: Option<(u64, u32, super::super::Frame)>,
    last_asked: Instant,
}

/// The latest frame of the stream, written by the reader thread.
#[derive(Default)]
struct Latest {
    /// Shared, so readers take the frame without copying megabytes.
    data: Arc<Vec<u8>>,
    width: usize,
    height: usize,
    seq: u64,
    at: Option<Instant>,
    ended: bool,
}

type Shared = Arc<(Mutex<Latest>, Condvar)>;

/// One gst-launch-1.0 reading the session's PipeWire node into a FIFO as raw BGRx frames; the
/// frame size comes from its `-v` caps on stdout.
struct FrameStream {
    child: Child,
    fifo: PathBuf,
    shared: Shared,
}

impl FrameStream {
    fn start(node_id: u32, size: Option<(u32, u32)>) -> Result<Self> {
        Self::spawn(|fifo| {
            let mut command = Command::new("gst-launch-1.0");
            command.args(pipeline_args(node_id, size, fifo));
            command
        })
        .context("gst-launch-1.0 (install gstreamer1.0-tools and gstreamer1.0-pipewire)")
    }

    /// Run the frame writer `command` makes for the FIFO: it prints GStreamer's `-v` caps on
    /// stdout and writes raw BGRx frames into the FIFO.
    fn spawn(command: impl FnOnce(&std::path::Path) -> Command) -> Result<Self> {
        // One FIFO per stream: a restarted stream, or a test's, never opens another's.
        static STREAMS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = STREAMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
        let fifo = std::path::Path::new(&dir)
            .join(format!("rtx-control-frames-{}-{n}.bgrx", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())?;
        // SAFETY: a valid NUL-terminated path.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            bail!("mkfifo: {}", std::io::Error::last_os_error());
        }
        let mut command = command(&fifo);
        die_with_parent(&mut command);
        let mut child = match command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&fifo);
                return Err(error.into());
            }
        };
        let shared: Shared = Arc::default();

        let stdout = child.stdout.take().expect("piped stdout");
        let caps = Arc::clone(&shared);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
                if !line.contains("GstFileSink") {
                    continue;
                }
                if let Some((width, height)) = frame_size_from_caps(&line) {
                    let mut latest = caps.0.lock().unwrap();
                    latest.width = width;
                    latest.height = height;
                }
            }
        });

        let reader = Arc::clone(&shared);
        let path = fifo.clone();
        std::thread::spawn(move || {
            let read = (|| -> Result<()> {
                let mut file = File::open(&path)?;
                // A frame is megabytes; the default 64 KiB pipe takes ~140 reads per 1920x1200
                // frame, and that copying was half the helper's CPU in a live view (#2683).
                // Best effort: the limit is /proc/sys/fs/pipe-max-size (1 MiB by default).
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_SIZE) };
                loop {
                    let mut size = (0, 0);
                    // The caps line comes before the first frame; wait for it.
                    let started = Instant::now();
                    while size.0 == 0 {
                        {
                            let latest = reader.0.lock().unwrap();
                            size = (latest.width, latest.height);
                        }
                        if size.0 == 0 {
                            if started.elapsed() > FRAME_TIMEOUT {
                                bail!("no frame size from GStreamer");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    // Read into reserved, not zeroed, memory: zeroing every frame first cost
                    // as much again.
                    let len = size.0 * size.1 * 4;
                    let mut data = Vec::with_capacity(len);
                    (&mut file).take(len as u64).read_to_end(&mut data)?;
                    if data.len() < len {
                        bail!("the screen stream ended mid-frame");
                    }
                    let (lock, ready) = &*reader;
                    let mut latest = lock.lock().unwrap();
                    latest.data = Arc::new(data);
                    latest.seq += 1;
                    latest.at = Some(Instant::now());
                    ready.notify_all();
                }
            })();
            let _ = read;
            let (lock, ready) = &*reader;
            lock.lock().unwrap().ended = true;
            ready.notify_all();
        });

        Ok(Self {
            child,
            fifo,
            shared,
        })
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None)) && !self.shared.0.lock().unwrap().ended
    }

    /// Wait until a frame newer than `after` arrives (true), or `timeout` passes.
    fn wait_newer(&self, after: u64, timeout: Duration) -> bool {
        let (lock, ready) = &*self.shared;
        let deadline = Instant::now() + timeout;
        let mut latest = lock.lock().unwrap();
        while latest.seq <= after && !latest.ended {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            latest = ready.wait_timeout(latest, left).unwrap().0;
        }
        latest.seq > after
    }

    fn seq(&self) -> u64 {
        self.shared.0.lock().unwrap().seq
    }

    /// Wait up to `timeout` for the first frame and return its size, calling `poke` between
    /// waits (a pointer nudge makes Mutter send one). Fails as soon as the writer exits or the
    /// stream ends without one.
    fn wait_ready(&mut self, timeout: Duration, mut poke: impl FnMut()) -> Result<(usize, usize)> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let latest = self.shared.0.lock().unwrap();
                if latest.seq > 0 {
                    return Ok((latest.width, latest.height));
                }
            }
            if !self.running() {
                bail!("the screen stream ended before its first frame");
            }
            if Instant::now() >= deadline {
                bail!("no frame from the screen stream in {timeout:?}");
            }
            poke();
            self.wait_newer(0, READY_POLL);
        }
    }
}

/// SIGKILL to the child when the thread that started it dies (#2670). `Drop` stops the pipeline on
/// a normal exit, but not when rtx-control itself is killed (a dev-server restart, a crash), which
/// left `gst-launch-1.0` running under systemd, holding a screencast stream. The pipeline is
/// started on the control loop's (main) thread, which lives as long as the process; the same
/// pattern starts the recorder's export ffmpeg.
fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls between fork and exec: no allocation, locks or logging.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent died before the signal was armed: nothing would ever send it.
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A reader still waiting to open the FIFO opens it now and reads its end.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.fifo);
        let _ = std::fs::remove_file(&self.fifo);
    }
}

impl WaylandInput {
    /// A session recording every connected monitor, else `virtualDisplays` virtual ones.
    pub fn new() -> Result<Self> {
        let conn = Connection::session().context("session D-Bus")?;
        let manager = Proxy::new(&conn, RD_BUS, "/org/gnome/Mutter/RemoteDesktop", RD_BUS)
            .context("Mutter RemoteDesktop (is this GNOME on Wayland?)")?;
        let path: OwnedObjectPath = manager.call("CreateSession", &())?;
        let session = Proxy::new_owned(conn.clone(), RD_BUS, path, RD_SESSION)?;
        let session_id: String = session.get_property("SessionId")?;

        let screencast = Proxy::new(&conn, SC_BUS, "/org/gnome/Mutter/ScreenCast", SC_BUS)?;
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert(
            "remote-desktop-session-id",
            Value::from(session_id.as_str()),
        );
        let sc_path: OwnedObjectPath = screencast.call("CreateSession", &(options,))?;
        let sc_session = Proxy::new_owned(conn.clone(), SC_BUS, sc_path, SC_SESSION)?;
        let record = || -> HashMap<&str, Value> {
            HashMap::from([("cursor-mode", Value::U32(CURSOR_EMBEDDED))])
        };
        // Which monitors: an unreadable DisplayConfig records the primary alone, as before.
        let monitors = monitors(&conn).unwrap_or_default();
        let chosen = to_record(&monitors);
        let mut streams: Vec<(DisplayInfo, OwnedObjectPath, Option<(u32, u32)>)> = Vec::new();
        // An empty connector records the primary monitor, as before #12. With no monitor
        // attached there is none: record virtual ones, of the size the frame pipeline asks for
        // (#9). As platform monitors, the desktop lays out on them as on real ones.
        let headless = match sc_session.call::<_, _, OwnedObjectPath>("RecordMonitor", &("", record())) {
            Ok(path) => {
                let primary = primary_of(&monitors);
                streams.push((primary.clone(), path, None));
                for info in chosen.iter().filter(|m| m.id != primary.id) {
                    match sc_session.call("RecordMonitor", &(info.id.as_str(), record())) {
                        Ok(path) => streams.push((info.clone(), path, None)),
                        // Gone since it was listed: the next list starts over without it.
                        Err(error) => eprintln!("[wayland] {} not recorded: {error}", info.id),
                    }
                }
                false
            }
            Err(error) if no_monitor(&error.to_string()) => {
                for info in virtual_displays(VIRTUAL_DISPLAYS.load(Ordering::Relaxed)) {
                    let mut properties = record();
                    properties.insert("is-platform", Value::Bool(true));
                    let path = sc_session
                        .call("RecordVirtual", &(properties,))
                        .context("no monitor attached, and Mutter recorded no virtual one")?;
                    streams.push((info, path, Some(VIRTUAL_SIZE)));
                }
                true
            }
            Err(error) => return Err(error.into()),
        };

        // Each stream's PipeWire node, once the session starts.
        let mut nodes = Vec::new();
        for (_, path, _) in &streams {
            let stream = Proxy::new_owned(conn.clone(), SC_BUS, path.clone(), SC_STREAM)?;
            let (tx, rx) = mpsc::channel();
            let mut added = stream.receive_signal("PipeWireStreamAdded")?;
            std::thread::spawn(move || {
                if let Some(message) = added.next() {
                    let _ = tx.send(message.body().deserialize::<u32>());
                }
            });
            nodes.push(rx);
        }
        session.call::<_, _, ()>("Start", &())?;
        let started = Instant::now();
        let mut recorded = Vec::new();
        for ((info, stream_path, virtual_size), rx) in streams.into_iter().zip(nodes) {
            let left = STREAM_TIMEOUT.saturating_sub(started.elapsed());
            let node_id = rx
                .recv_timeout(left)
                .map_err(|_| anyhow!("GNOME did not start the screen stream"))??;
            recorded.push(Recorded {
                info,
                stream_path,
                node_id,
                virtual_size,
                frames: None,
                frame_size: None,
                last_frame: None,
                last_asked: Instant::now(),
            });
        }

        let mut input = Self {
            session,
            recorded,
            headless,
            connectors: physical(&monitors),
            monitors_changed: Arc::new(Mutex::new(None)),
            placed_at: Some(Instant::now()),
            clipboard_data: Arc::new(Mutex::new(HashMap::new())),
            clipboard_enabled: false,
            selection: Arc::new(Mutex::new(Selection::default())),
        };
        // A virtual monitor exists only once its stream is negotiated and read, and Mutter lays
        // the monitors out again as each appears: read every one from the start until its first
        // frame, so all exist (and the first input has a monitor to land on, scaled to its
        // size). A monitor outlives its reader, so the pipelines then idle-stop like others. A
        // failure here drops `input`, which stops the session.
        for index in 0..input.recorded.len() {
            let Some(size) = input.recorded[index].virtual_size else {
                continue;
            };
            let mut frames = FrameStream::start(input.recorded[index].node_id, Some(size))?;
            let (width, height) = frames.wait_ready(FRAME_TIMEOUT, || {
                let _ = input.nudge_on(index);
            })?;
            input.recorded[index].frame_size = Some((width as u32, height as u32));
            input.recorded[index].frames = Some(frames);
        }
        if input.headless {
            input.place_virtual(&conn);
        }
        input.watch_monitors(&conn);
        Ok(input)
    }

    /// Note each MonitorsChanged (a monitor plugged in or out, or Link's own virtual monitors
    /// appearing); [`Self::restart_due`] decides what it means.
    fn watch_monitors(&self, conn: &Connection) {
        let Ok(config) = Proxy::new(conn, DC_BUS, "/org/gnome/Mutter/DisplayConfig", DC_BUS) else {
            return;
        };
        let Ok(mut changes) = config.receive_signal("MonitorsChanged") else {
            return;
        };
        let noted = Arc::downgrade(&self.monitors_changed);
        std::thread::spawn(move || {
            for _ in &mut changes {
                let Some(noted) = noted.upgrade() else { return };
                *noted.lock().unwrap() = Some(Instant::now());
            }
        });
    }

    /// The virtual monitors' places and sizes as Mutter laid them out: `Meta-n` in its own
    /// numbering, matched to this session's in the order they were made. Their ids stay this
    /// session's (`Meta-0`… by creation), so a new session keeps them for the same monitors.
    fn place_virtual(&mut self, conn: &Connection) {
        let Ok(listed) = monitors(conn) else { return };
        let mut placed: Vec<DisplayInfo> = listed.into_iter().filter(|m| is_virtual(&m.id)).collect();
        placed.sort_by_key(|m| connector_number(&m.id));
        for (recorded, place) in self.recorded.iter_mut().zip(placed) {
            recorded.info.x = place.x;
            recorded.info.y = place.y;
            recorded.info.width = place.width;
            recorded.info.height = place.height;
            recorded.info.scale = place.scale;
        }
    }

    /// Whether the physical monitors changed since this session started: a MonitorsChanged
    /// that has settled for RESTART_SETTLE, and another set of physical connectors now (Link's
    /// own virtual monitors coming and going don't count). A change that isn't one is forgotten.
    fn restart_due(&self, conn: &Connection) -> bool {
        let noted = *self.monitors_changed.lock().unwrap();
        if !settled(noted, Instant::now()) {
            return false;
        }
        *self.monitors_changed.lock().unwrap() = None;
        match monitors(conn) {
            Ok(now) => physical(&now) != self.connectors,
            Err(_) => false,
        }
    }

    /// The listed places of the monitors and which one is the primary, which may change
    /// without a new session.
    fn replace_monitors(&mut self, now: &[DisplayInfo]) {
        replace_places(
            self.recorded
                .iter_mut()
                .filter(|r| r.virtual_size.is_none())
                .map(|r| &mut r.info),
            now,
        );
    }

    /// Read the monitors again when the last read is 2 s old (or `now`), after a new session if
    /// the monitors changed. Capture and the pointer both
    /// resolve "the primary" from what this leaves (review of !15, finding 1).
    fn refresh(&mut self, now: bool) -> Result<()> {
        self.restart_if_due()?;
        let stale = self
            .placed_at
            .is_none_or(|at| at.elapsed() >= crate::displays::REFRESH);
        if !self.headless && (now || stale) {
            let conn = self.session.connection().clone();
            if let Ok(listed) = monitors(&conn) {
                self.replace_monitors(&listed);
            }
            self.placed_at = Some(Instant::now());
        }
        Ok(())
    }

    /// The displays as this session resolves them now, for capture and the pointer.
    pub fn current(&mut self) -> Result<Vec<Display>> {
        self.refresh(false)?;
        Ok(self.listed())
    }

    /// The primary display's frame pixels; known after the first screenshot.
    pub fn screen_size(&self) -> Option<(f64, f64)> {
        self.recorded
            .first()
            .and_then(|r| r.frame_size)
            .map(|(w, h)| (w as f64, h as f64))
    }

    /// The displays (realtimex-link#12), each with its stream's pixels as the rectangle the
    /// pointer takes. When the connected monitors are not the ones this session was made for
    /// (one plugged in or out), the session starts again with the new set first.
    pub fn displays(&mut self) -> Result<Vec<Display>> {
        self.refresh(true)?;
        Ok(self.listed())
    }

    /// A new session for the monitors connected now, when they changed. Views on displays that
    /// still exist keep them (same ids); rtx-linkd moves the others to the primary.
    fn restart_if_due(&mut self) -> Result<()> {
        let conn = self.session.connection().clone();
        if self.restart_due(&conn) {
            eprintln!("[wayland] the monitors changed: starting the session again");
            // The old session stops first.
            self.stop_session();
            *self = Self::new()?;
        }
        Ok(())
    }

    /// The displays this session records, as it knows them.
    fn listed(&self) -> Vec<Display> {
        self.recorded
            .iter()
            .map(|r| {
                let (width, height) = r.frame_size.unwrap_or((r.info.width, r.info.height));
                Display {
                    info: r.info.clone(),
                    rect: Rect {
                        x: 0.0,
                        y: 0.0,
                        width: width as f64,
                        height: height as f64,
                    },
                }
            })
            .collect()
    }

    fn stop_session(&mut self) {
        for recorded in &mut self.recorded {
            recorded.frames = None;
        }
        let _ = self.session.call::<_, _, ()>("Stop", &());
    }

    /// The recorded display `display` names (None: the current primary, as `current` says),
    /// in a new session first when the monitors changed.
    fn index_of(&mut self, display: Option<&str>) -> Result<usize> {
        self.refresh(false)?;
        stream_for(self.recorded.iter().map(|r| &r.info), display).ok_or_else(|| {
            DisplayNotFound(display.unwrap_or("primary").to_string()).into()
        })
    }

    /// Stop the frame pipelines no request used for STREAM_IDLE (a virtual monitor outlives
    /// its reader, and a new one gets frames again).
    fn stop_idle(&mut self, keep: usize) {
        for (index, recorded) in self.recorded.iter_mut().enumerate() {
            if index != keep
                && recorded.frames.is_some()
                && recorded.last_asked.elapsed() >= STREAM_IDLE
            {
                recorded.frames = None;
                recorded.last_frame = None;
            }
        }
    }

    /// Mutter streams a frame on pointer motion, not when an app repaints: a 1px nudge and back
    /// makes the next frame current.
    fn nudge(&self) -> Result<()> {
        self.session
            .call::<_, _, ()>("NotifyPointerMotionRelative", &(1.0f64, 0.0f64))?;
        self.session
            .call::<_, _, ()>("NotifyPointerMotionRelative", &(-1.0f64, 0.0f64))?;
        Ok(())
    }

    /// A nudge that reaches stream `index` wherever the pointer is: Mutter sends a monitor's
    /// first frame on pointer motion over it, so a reader attached to a display the pointer
    /// isn't on might wait in vain (T30 spike). The pointer goes to that display's middle.
    fn nudge_on(&self, index: usize) -> Result<()> {
        let recorded = &self.recorded[index];
        let (width, height) = recorded
            .frame_size
            .or(recorded.virtual_size)
            .unwrap_or((recorded.info.width, recorded.info.height));
        self.session.call::<_, _, ()>(
            "NotifyPointerMotionAbsolute",
            &(recorded.stream_path.as_str(), width as f64 / 2.0, height as f64 / 2.0),
        )?;
        Ok(())
    }

    /// The latest frame of `display`'s stream (None: the primary), one newer than this request
    /// when the screen is changing (an action's effect shows), or else the latest. A `live`
    /// view does not wait: it nudges for the next frame and takes what is there, at most one
    /// frame behind.
    pub fn screenshot(&mut self, max_width: u32, live: bool, display: Option<&str>) -> Result<Shot> {
        let index = self.index_of(display)?;
        let (data, width, height) = self.latest_frame(index, live, false)?;
        super::x11::shot(
            &data[..width * height * 4],
            width,
            height,
            max_width,
            "display".into(),
        )
    }

    /// The latest picture of `display`'s stream as a live-view video frame: no waiting for a
    /// newer one (the stream sends a frame when the screen changes).
    /// Until the stream sends a newer frame, the last one comes back as it was: not copied or
    /// scaled again, and with the same number, so the encoder skips it (#2683). The live view asks
    /// 30 times a second, and GNOME sends a frame only when the screen changes.
    pub fn frame(&mut self, max_width: u32, display: Option<&str>) -> Result<super::super::Frame> {
        let index = self.index_of(display)?;
        let recorded = &mut self.recorded[index];
        recorded.last_asked = Instant::now();
        if let Some((seq, width, frame)) = &recorded.last_frame {
            let unchanged = recorded
                .frames
                .as_mut()
                .is_some_and(|f| f.running() && f.seq() == *seq);
            if unchanged && *width == max_width {
                return Ok(frame.clone());
            }
        }
        let (data, width, height, stream_seq) = self.numbered_frame(index, false, true)?;
        let frame = super::super::Frame {
            seq: super::super::next_frame_seq(),
            ..super::super::Frame::scaled(
                &rtx_screens::image::Bgra {
                    width,
                    height,
                    stride: width * 4,
                    data: &data,
                },
                max_width,
            )
        };
        self.recorded[index].last_frame = Some((stream_seq, max_width, frame.clone()));
        Ok(frame)
    }

    /// BGRA of the latest frame of stream `index`. The first one, a `live` view's and a fresh
    /// screenshot's wait for a newer frame as before; a video frame (`frame`) takes what is
    /// there.
    fn latest_frame(
        &mut self,
        index: usize,
        live: bool,
        video: bool,
    ) -> Result<(Arc<Vec<u8>>, usize, usize)> {
        let (data, width, height, _) = self.numbered_frame(index, live, video)?;
        Ok((data, width, height))
    }

    /// [`Self::latest_frame`] with the stream's number for it.
    fn numbered_frame(
        &mut self,
        index: usize,
        live: bool,
        video: bool,
    ) -> Result<(Arc<Vec<u8>>, usize, usize, u64)> {
        self.stop_idle(index);
        let recorded = &mut self.recorded[index];
        recorded.last_asked = Instant::now();
        if !recorded.frames.as_mut().is_some_and(FrameStream::running) {
            recorded.frames = None;
            // A new stream numbers its frames from 1 again: nothing cached may match them.
            recorded.last_frame = None;
            recorded.frames = Some(FrameStream::start(recorded.node_id, recorded.virtual_size)?);
        }
        let before = self.recorded[index].frames.as_ref().unwrap().seq();
        if before == 0 {
            // A new reader: its own display's first frame.
            self.nudge_on(index)?;
            if !self.recorded[index]
                .frames
                .as_ref()
                .unwrap()
                .wait_newer(0, FRAME_TIMEOUT)
            {
                self.recorded[index].frames = None;
                bail!("no frame from the screen stream in {FRAME_TIMEOUT:?}");
            }
        } else if live {
            self.nudge()?;
            self.recorded[index]
                .frames
                .as_ref()
                .unwrap()
                .wait_newer(before, LIVE_FRAME_WAIT);
        } else if !video
            && !self.recorded[index]
                .frames
                .as_ref()
                .unwrap()
                .wait_newer(before, FRESH_FRAME_WAIT)
        {
            self.nudge()?;
            self.recorded[index]
                .frames
                .as_ref()
                .unwrap()
                .wait_newer(before, FRESH_FRAME_WAIT);
        }
        let recorded = &mut self.recorded[index];
        let frames = recorded.frames.as_ref().unwrap();
        let (data, width, height, seq) = {
            let latest = frames.shared.0.lock().unwrap();
            if latest.data.is_empty() {
                bail!("the screen stream ended");
            }
            (latest.data.clone(), latest.width, latest.height, latest.seq)
        };
        if data.len() < width * height * 4 {
            bail!("short frame: {} bytes for {width}x{height}", data.len());
        }
        recorded.frame_size = Some((width as u32, height as u32));
        Ok((data, width, height, seq))
    }

    /// The pointer to (x, y) in `display`'s stream pixels.
    pub fn move_to(&mut self, display: &Display, x: f64, y: f64) -> Result<()> {
        let index = self.index_of(Some(&display.info.id))?;
        let path = self.recorded[index].stream_path.clone();
        self.session
            .call::<_, _, ()>("NotifyPointerMotionAbsolute", &(path.as_str(), x, y))?;
        Ok(())
    }

    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.session
            .call::<_, _, ()>("NotifyPointerButton", &(evdev_button(button), pressed))?;
        Ok(())
    }

    /// A physical key by its evdev code (Mutter's NotifyKeyboardKeycode): the session's keymap
    /// and input method decide what it types.
    pub fn key(&mut self, evdev: u16, pressed: bool) -> Result<()> {
        self.session
            .call::<_, _, ()>("NotifyKeyboardKeycode", &(u32::from(evdev), pressed))?;
        Ok(())
    }

    pub fn keysym(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        self.session
            .call::<_, _, ()>("NotifyKeyboardKeysym", &(keysym, pressed))?;
        Ok(())
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        if dy != 0 {
            self.session
                .call::<_, _, ()>("NotifyPointerAxisDiscrete", &(0u32, dy))?;
        }
        if dx != 0 {
            self.session
                .call::<_, _, ()>("NotifyPointerAxisDiscrete", &(1u32, dx))?;
        }
        Ok(())
    }

    /// Own the clipboard (answering SelectionTransfer on a thread) and press ctrl+v, or the
    /// terminal's paste shortcut in a terminal. Keystrokes would pass through the input method.
    fn enable_clipboard(&mut self) -> Result<()> {
        if self.clipboard_enabled {
            return Ok(());
        }
        let mut transfers = self.session.receive_signal("SelectionTransfer")?;
        let session = self.session.clone();
        let data = Arc::clone(&self.clipboard_data);
        std::thread::spawn(move || {
            for message in &mut transfers {
                let Ok((mime, serial)) = message.body().deserialize::<(String, u32)>() else {
                    continue;
                };
                let written = (|| -> Result<()> {
                    let fd: OwnedFd = session.call("SelectionWrite", &(serial,))?;
                    let mut file = File::from(std::os::fd::OwnedFd::from(fd));
                    let bytes = data.lock().unwrap().get(&mime).cloned().unwrap_or_default();
                    file.write_all(&bytes)?;
                    Ok(())
                })();
                let _ = session.call::<_, _, ()>("SelectionWriteDone", &(serial, written.is_ok()));
            }
        });
        // Who owns the clipboard now: counted for a viewer's "copied there" (#2682).
        let mut owners = self.session.receive_signal("SelectionOwnerChanged")?;
        let selection = Arc::clone(&self.selection);
        std::thread::spawn(move || {
            for message in &mut owners {
                let Ok((options,)) = message
                    .body()
                    .deserialize::<(HashMap<String, OwnedValue>,)>()
                else {
                    continue;
                };
                let mime_types: Vec<String> =
                    options.get("mime-types").map(mime_list).unwrap_or_default();
                let own = options
                    .get("session-is-owner")
                    .and_then(|value| bool::try_from(value.clone()).ok())
                    .unwrap_or(false);
                let mut current = selection.lock().unwrap();
                current.changes += 1;
                current.mime_types = mime_types;
                current.own = own;
            }
        });
        let options: HashMap<&str, Value> = HashMap::new();
        self.session
            .call::<_, _, ()>("EnableClipboard", &(options,))?;
        self.clipboard_enabled = true;
        Ok(())
    }

    /// How many times the clipboard changed hands, and whether this session holds it now.
    pub fn clipboard_seq(&mut self) -> Result<(u64, bool)> {
        self.enable_clipboard()?;
        let current = self.selection.lock().unwrap();
        Ok((current.changes, current.own))
    }

    /// What the clipboard holds: files, a PNG or text, read through GNOME (SelectionRead).
    pub fn clipboard_read(&mut self) -> Result<crate::platform::ClipboardContent> {
        use crate::platform::ClipboardContent;
        self.enable_clipboard()?;
        let mime_types = self.selection.lock().unwrap().mime_types.clone();
        if std::env::var_os("RTX_CLIPBOARD_DEBUG").is_some() {
            eprintln!("[clipboard] offered: {mime_types:?}");
        }
        // Unknown types (an announcement without them): try each, GNOME refuses what is not there.
        let offered = |mime: &str| mime_types.is_empty() || mime_types.iter().any(|m| m == mime);
        for mime in ["x-special/gnome-copied-files", "text/uri-list"] {
            if offered(mime) {
                let Ok(bytes) = self.selection_read(mime) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let paths = super::x11_clipboard::paths_of_uri_text(&text);
                if !paths.is_empty() {
                    return Ok(ClipboardContent::Files(paths));
                }
            }
        }
        if offered("image/png") {
            if let Ok(bytes) = self.selection_read("image/png") {
                if !bytes.is_empty() {
                    return Ok(ClipboardContent::Image {
                        bytes,
                        mime: "image/png",
                    });
                }
            }
        }
        for mime in [
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "text/plain",
            "STRING",
        ] {
            if offered(mime) {
                if let Ok(bytes) = self.selection_read(mime) {
                    if !bytes.is_empty() {
                        return Ok(ClipboardContent::Text(
                            String::from_utf8_lossy(&bytes).into_owned(),
                        ));
                    }
                }
            }
        }
        Ok(ClipboardContent::Empty)
    }

    /// The clipboard's data in one MIME type: GNOME hands a pipe the owner writes into.
    fn selection_read(&self, mime: &str) -> Result<Vec<u8>> {
        let read = self.selection_read_once(mime);
        if let Err(error) = &read {
            eprintln!("[clipboard] reading {mime} failed: {error:#}");
        }
        read
    }

    fn selection_read_once(&self, mime: &str) -> Result<Vec<u8>> {
        let fd: OwnedFd = self.session.call("SelectionRead", &(mime,))?;
        let mut file = File::from(std::os::fd::OwnedFd::from(fd));
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            // GNOME's pipe is non-blocking: wait for more until the owner closes it.
            let deadline = Instant::now() + SELECTION_READ_TIMEOUT;
            let mut data = Vec::new();
            let mut buffer = [0u8; 64 * 1024];
            let read = loop {
                match file.read(&mut buffer) {
                    Ok(0) => break Ok(data),
                    Ok(n) => data.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            break Err(error);
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => break Err(error),
                }
            };
            let _ = done.send(read);
        });
        match result.recv_timeout(SELECTION_READ_TIMEOUT) {
            Ok(read) => Ok(read?),
            Err(_) => bail!("the clipboard owner did not answer"),
        }
    }

    pub fn paste_text(&mut self, text: &str, terminal: bool) -> Result<()> {
        self.offer(
            TEXT_MIME_TYPES
                .iter()
                .map(|mime| (*mime, text.as_bytes().to_vec()))
                .collect(),
        )?;
        self.press_paste(terminal)
    }

    /// Put a PNG on the clipboard and press ctrl+v.
    pub fn paste_image(&mut self, png: &[u8]) -> Result<()> {
        self.offer(vec![("image/png", png.to_vec())])?;
        self.press_paste(false)
    }

    /// Put these files on the clipboard (as Files copies them) and press ctrl+v.
    pub fn paste_files(&mut self, paths: &[String]) -> Result<()> {
        let uris: Vec<String> = paths.iter().map(|p| super::file_uri(p)).collect();
        let uri_list = uris
            .iter()
            .map(|uri| format!("{uri}\r\n"))
            .collect::<String>();
        let gnome = format!("copy\n{}", uris.join("\n"));
        let text = paths.join("\n");
        self.offer(vec![
            ("x-special/gnome-copied-files", gnome.into_bytes()),
            ("text/uri-list", uri_list.into_bytes()),
            ("text/plain;charset=utf-8", text.clone().into_bytes()),
            ("text/plain", text.into_bytes()),
        ])?;
        self.press_paste(false)
    }

    /// Announce these MIME types and their bytes as the clipboard.
    fn offer(&mut self, formats: Vec<(&str, Vec<u8>)>) -> Result<()> {
        self.enable_clipboard()?;
        let mime_types: Vec<&str> = formats.iter().map(|(mime, _)| *mime).collect();
        *self.clipboard_data.lock().unwrap() = formats
            .into_iter()
            .map(|(mime, bytes)| (mime.to_string(), bytes))
            .collect();
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("mime-types", Value::from(mime_types));
        self.session.call::<_, _, ()>("SetSelection", &(options,))?;
        Ok(())
    }

    /// Ctrl+V, or in a terminal (asked for, or the active window is one by AT-SPI) that
    /// terminal's own paste shortcut, Ctrl+Shift+V unless the person changed it (#2747).
    fn press_paste(&mut self, in_terminal: bool) -> Result<()> {
        let keys = match terminal::focused_terminal_by_atspi() {
            Some(exe) => terminal::paste_keys(&exe),
            None if in_terminal => vec![KEYSYM_CTRL, KEYSYM_SHIFT, KEYSYM_V],
            None => vec![KEYSYM_CTRL, KEYSYM_V],
        };
        std::thread::sleep(PASTE_SETTLE);
        for &keysym in &keys {
            self.keysym(keysym, true)?;
        }
        for &keysym in keys.iter().rev() {
            self.keysym(keysym, false)?;
        }
        std::thread::sleep(PASTE_SETTLE);
        Ok(())
    }
}

impl Drop for WaylandInput {
    fn drop(&mut self) {
        self.stop_session();
    }
}

type MonitorSpec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type PhysicalMonitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, HashMap<String, OwnedValue>);
/// Mutter's `org.gnome.Mutter.DisplayConfig.GetCurrentState` answer.
type CurrentState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);

/// The connected monitors, in the picker's order: one per logical monitor (mirrored monitors
/// show one picture), by connector.
fn monitors(conn: &Connection) -> Result<Vec<DisplayInfo>> {
    let config = Proxy::new(conn, DC_BUS, "/org/gnome/Mutter/DisplayConfig", DC_BUS)?;
    let state: CurrentState = config.call("GetCurrentState", &())?;
    Ok(monitors_of(&state))
}

fn flag(value: Option<&OwnedValue>) -> bool {
    value.and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

/// The displays a DisplayConfig state describes: each logical monitor's first monitor, at the
/// logical monitor's place, its current mode's pixels (turned with it) and scale.
fn monitors_of(state: &CurrentState) -> Vec<DisplayInfo> {
    let (_, physical, logical, _) = state;
    let mut list: Vec<DisplayInfo> = logical
        .iter()
        .filter_map(|(x, y, scale, transform, primary, specs, _)| {
            let spec = specs.first()?;
            let (_, modes, properties) = physical.iter().find(|(s, _, _)| s.0 == spec.0)?;
            let mode = modes
                .iter()
                .find(|mode| flag(mode.6.get("is-current")))
                .or(modes.first())?;
            let (mut width, mut height) = (mode.1.max(0) as u32, mode.2.max(0) as u32);
            // 90° and 270°, flipped or not.
            if transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            let name = properties
                .get("display-name")
                .and_then(|v| <&str>::try_from(v).ok())
                .filter(|name| *name != VIRTUAL_VENDOR)
                .unwrap_or_default()
                .to_string();
            Some(DisplayInfo {
                id: spec.0.clone(),
                name,
                primary: *primary,
                x: *x,
                y: *y,
                width,
                height,
                scale: *scale,
            })
        })
        .collect();
    rtx_screens::sort_displays(&mut list);
    list
}

/// The primary monitor, else the first; a stand-in when DisplayConfig can't be read (the
/// session still records the primary, as before #12).
fn primary_of(monitors: &[DisplayInfo]) -> DisplayInfo {
    monitors
        .iter()
        .find(|m| m.primary)
        .or(monitors.first())
        .cloned()
        .unwrap_or(DisplayInfo {
            id: "primary".into(),
            name: String::new(),
            primary: true,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            scale: 1.0,
        })
}

/// Bring the recorded monitors' places and primary flags up to date with `now` (a monitor
/// not listed keeps its last; its stream only freezes until the next session).
fn replace_places<'a>(recorded: impl Iterator<Item = &'a mut DisplayInfo>, now: &[DisplayInfo]) {
    for info in recorded {
        match now.iter().find(|m| m.id == info.id) {
            Some(place) => *info = place.clone(),
            None => info.primary = false,
        }
    }
}

/// Which recorded stream a request means: the display named, or for none the one flagged
/// primary now (else the first). Stream order is the session's, not the primary's.
fn stream_for<'a>(
    recorded: impl Iterator<Item = &'a DisplayInfo> + Clone,
    display: Option<&str>,
) -> Option<usize> {
    match display {
        Some(id) => recorded.clone().position(|info| info.id == id),
        None => recorded
            .clone()
            .position(|info| info.primary)
            .or_else(|| recorded.clone().next().map(|_| 0)),
    }
}

/// The monitors a new session records: every one, the primary first.
fn to_record(monitors: &[DisplayInfo]) -> Vec<DisplayInfo> {
    let primary = primary_of(monitors);
    let mut chosen = vec![primary.clone()];
    chosen.extend(monitors.iter().filter(|m| m.id != primary.id).cloned());
    chosen
}

/// Whether a connector is one of Mutter's virtual monitors (Link's own, when headless).
fn is_virtual(connector: &str) -> bool {
    connector.starts_with(VIRTUAL_CONNECTOR)
}

/// The number in a `Meta-n` connector, for Mutter's order of its virtual monitors.
fn connector_number(connector: &str) -> u32 {
    connector
        .strip_prefix(VIRTUAL_CONNECTOR)
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// The physical monitors' connectors, sorted: what a new session would be made for.
fn physical(monitors: &[DisplayInfo]) -> Vec<String> {
    let mut connectors: Vec<String> = monitors
        .iter()
        .map(|m| m.id.clone())
        .filter(|id| !is_virtual(id))
        .collect();
    connectors.sort();
    connectors
}

/// Whether a MonitorsChanged noted at `noted` has settled by `now` (none came since for
/// RESTART_SETTLE).
fn settled(noted: Option<Instant>, now: Instant) -> bool {
    noted.is_some_and(|at| now.saturating_duration_since(at) >= RESTART_SETTLE)
}

/// `count` virtual monitors, `Meta-0` (the primary) on, by creation order, side by side as Mutter
/// lays them out (their places are read again once they exist).
fn virtual_displays(count: u32) -> Vec<DisplayInfo> {
    (1..=count.clamp(1, 4))
        .map(|n| DisplayInfo {
            id: format!("{VIRTUAL_CONNECTOR}{}", n - 1),
            name: String::new(),
            primary: n == 1,
            x: (n as i32 - 1) * VIRTUAL_SIZE.0 as i32,
            y: 0,
            width: VIRTUAL_SIZE.0,
            height: VIRTUAL_SIZE.1,
            scale: 1.0,
        })
        .collect()
}

/// Mutter's answer to RecordMonitor when there is no monitor to record (none attached).
fn no_monitor(error: &str) -> bool {
    error.contains("Unknown monitor")
}

/// gst-launch-1.0's arguments: the session's PipeWire node into `fifo` as raw BGRx frames. A
/// virtual monitor's stream is held to `size` right at the source, so that is what it negotiates
/// and Mutter sizes the monitor to.
fn pipeline_args(node_id: u32, size: Option<(u32, u32)>, fifo: &std::path::Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["-v".into(), "pipewiresrc".into(), format!("path={node_id}")];
    // Timestamps from arrival and a frame-rate cap in the caps: `max-rate` alone let 30 full
    // frames a second (263 MB/s at 1920x1200) through on GNOME 46 (#2683).
    args.extend(["always-copy=true", "do-timestamp=true"].map(String::from));
    if let Some((width, height)) = size {
        args.extend(["!".into(), format!("video/x-raw,width={width},height={height}")]);
    }
    args.extend([
        "!".into(),
        "videorate".into(),
        "drop-only=true".into(),
        format!("max-rate={MAX_FRAME_RATE}"),
        "!".into(),
        format!("video/x-raw,framerate={MAX_FRAME_RATE}/1"),
        "!".into(),
        "videoconvert".into(),
        "!".into(),
        "video/x-raw,format=BGRx".into(),
        "!".into(),
        "filesink".into(),
        format!("location={}", fifo.display()),
        // Unbuffered: a whole frame reaches the reader as soon as GStreamer has it.
        "buffer-mode=unbuffered".into(),
        "sync=false".into(),
    ]);
    args
}

/// The negotiated size from `gst-launch-1.0 -v` output (the last caps line wins).
fn frame_size_from_caps(text: &str) -> Option<(usize, usize)> {
    text.lines().rev().find_map(|line| {
        let value = |key: &str| -> Option<usize> {
            let start = line.find(key)? + key.len();
            line[start..]
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        };
        Some((value("width=(int)")?, value("height=(int)")?))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        connector_number, die_with_parent, frame_size_from_caps, monitors_of, no_monitor, physical,
        pipeline_args, replace_places, settled, stream_for, to_record, virtual_displays,
        CurrentState, FrameStream, RESTART_SETTLE,
    };
    use std::collections::HashMap;
    use std::process::Command;
    use std::time::{Duration, Instant};
    use zbus::zvariant::{OwnedValue, Str};

    /// GetCurrentState with these monitors: (connector, name, x, y, mode, scale, transform,
    /// primary).
    fn state(monitors: &[(&str, &str, i32, i32, (i32, i32), f64, u32, bool)]) -> CurrentState {
        let spec = |c: &str| (c.to_string(), "GSM".to_string(), "LG".to_string(), "0x01".to_string());
        let physical = monitors
            .iter()
            .map(|(connector, name, _, _, (w, h), _, _, _)| {
                let current = HashMap::from([("is-current".to_string(), OwnedValue::from(true))]);
                let other = HashMap::new();
                (
                    spec(connector),
                    vec![
                        ("1024x768".to_string(), 1024, 768, 60.0, 1.0, vec![1.0], other),
                        (format!("{w}x{h}"), *w, *h, 60.0, 1.0, vec![1.0], current),
                    ],
                    HashMap::from([(
                        "display-name".to_string(),
                        OwnedValue::from(Str::from(name.to_string())),
                    )]),
                )
            })
            .collect();
        let logical = monitors
            .iter()
            .map(|(connector, _, x, y, _, scale, transform, primary)| {
                (*x, *y, *scale, *transform, *primary, vec![spec(connector)], HashMap::new())
            })
            .collect();
        (1, physical, logical, HashMap::new())
    }

    #[test]
    fn monitors_are_listed_from_display_config_left_to_right() {
        let list = monitors_of(&state(&[
            ("eDP-1", "Built-in display", 0, 0, (2880, 1800), 2.0, 0, true),
            ("HDMI-1", "DELL U2720Q", -1080, -200, (1920, 1080), 1.0, 1, false),
        ]));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "HDMI-1");
        assert_eq!(list[0].name, "DELL U2720Q");
        assert!(!list[0].primary);
        // Turned 90°: its current mode's pixels, turned.
        assert_eq!((list[0].x, list[0].y, list[0].width, list[0].height), (-1080, -200, 1080, 1920));
        assert_eq!(list[1].id, "eDP-1");
        assert!(list[1].primary);
        assert_eq!((list[1].width, list[1].height, list[1].scale), (2880, 1800, 2.0));
    }

    #[test]
    fn a_session_records_every_monitor_with_the_primary_first() {
        let monitors = monitors_of(&state(&[
            ("DP-1", "", -1920, 0, (1920, 1080), 1.0, 0, false),
            ("eDP-1", "", 0, 0, (1920, 1200), 1.0, 0, true),
            ("HDMI-1", "", 1920, 0, (1920, 1080), 1.0, 0, false),
        ]));
        let ids: Vec<String> = to_record(&monitors).into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["eDP-1", "DP-1", "HDMI-1"]);
    }

    /// C-W1: only physical monitors coming or going start the session again; Link's own
    /// virtual monitors (Meta-n) appearing and disappearing never do.
    #[test]
    fn only_physical_monitors_restart_the_session() {
        let at_start = physical(&monitors_of(&state(&[])));
        assert!(at_start.is_empty());
        // Headless: Link's two virtual monitors show up, one at a time, then one goes.
        for listed in [
            vec![("Meta-0", "MetaVendor", 0, 0, (1920, 1080), 1.0, 0, true)],
            vec![
                ("Meta-0", "MetaVendor", 0, 0, (1920, 1080), 1.0, 0, true),
                ("Meta-1", "MetaVendor", 1920, 0, (1920, 1080), 1.0, 0, false),
            ],
            vec![],
        ] {
            assert_eq!(physical(&monitors_of(&state(&listed))), at_start);
        }
        // A real monitor plugged in does.
        let plugged = monitors_of(&state(&[
            ("Meta-0", "MetaVendor", 0, 0, (1920, 1080), 1.0, 0, true),
            ("HDMI-1", "LG", 1920, 0, (1920, 1080), 1.0, 0, false),
        ]));
        assert_eq!(physical(&plugged), ["HDMI-1"]);
        // Order doesn't matter; a monitor that moved is the same set.
        let a = monitors_of(&state(&[
            ("eDP-1", "", 0, 0, (1920, 1200), 1.0, 0, true),
            ("HDMI-1", "", 1920, 0, (1920, 1080), 1.0, 0, false),
        ]));
        let b = monitors_of(&state(&[
            ("HDMI-1", "", -1920, 0, (1920, 1080), 1.0, 0, false),
            ("eDP-1", "", 0, 0, (1920, 1200), 1.0, 0, true),
        ]));
        assert_eq!(physical(&a), physical(&b));
        // Virtual monitors' vendor name is no name.
        assert_eq!(plugged.iter().find(|m| m.id == "Meta-0").unwrap().name, "");
    }

    /// Review of !15, finding 1: the primary changes (A to B) with both monitors attached. The
    /// session isn't made again; the list, the stream a screenshot or frame without a display
    /// reads, and the stream the pointer goes to all follow B.
    #[test]
    fn a_new_primary_is_shown_and_pointed_at_alike() {
        let before = monitors_of(&state(&[
            ("A", "", 0, 0, (1920, 1080), 1.0, 0, true),
            ("B", "", 1920, 0, (1920, 1080), 1.0, 0, false),
        ]));
        // The session's streams, primary first as made: A, B.
        let mut recorded = to_record(&before);
        assert_eq!(stream_for(recorded.iter(), None), Some(0));
        let after = monitors_of(&state(&[
            ("A", "", 0, 0, (1920, 1080), 1.0, 0, false),
            ("B", "", 1920, 0, (1920, 1080), 1.0, 0, true),
        ]));
        assert_eq!(physical(&before), physical(&after), "no new session");
        replace_places(recorded.iter_mut(), &after);
        // The list (`current`, what /displays and info say).
        let listed: Vec<crate::displays::Display> = recorded
            .iter()
            .map(|info| crate::displays::Display {
                info: info.clone(),
                rect: crate::displays::Rect::of(info),
            })
            .collect();
        let primary = crate::displays::find(&listed, None).unwrap();
        assert_eq!(primary.info.id, "B");
        // The capture source without a display, and the pointer's (it moves on the display the
        // controller resolved from that list).
        let shown = stream_for(recorded.iter(), None).unwrap();
        let pointed = stream_for(recorded.iter(), Some(&primary.info.id)).unwrap();
        assert_eq!(recorded[shown].id, "B");
        assert_eq!(shown, pointed);
        // A monitor no longer listed is not the primary any more.
        replace_places(recorded.iter_mut(), &after[1..]);
        assert_eq!(recorded[stream_for(recorded.iter(), None).unwrap()].id, "B");
    }

    /// C-W1: a change waits RESTART_SETTLE without another before it counts.
    #[test]
    fn a_monitor_change_counts_once_it_settles() {
        let now = Instant::now();
        assert!(!settled(None, now));
        assert!(!settled(Some(now), now));
        assert!(!settled(Some(now - RESTART_SETTLE / 2), now));
        assert!(settled(Some(now - RESTART_SETTLE), now));
        // Mutter's own numbering orders its virtual monitors.
        assert!(connector_number("Meta-2") < connector_number("Meta-10"));
        assert_eq!(connector_number("HDMI-1"), u32::MAX);
    }

    #[test]
    fn headless_records_virtual_monitors_side_by_side() {
        let one = virtual_displays(1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "Meta-0");
        assert!(one[0].primary);
        let three = virtual_displays(3);
        let places: Vec<(String, i32, bool)> =
            three.into_iter().map(|d| (d.id, d.x, d.primary)).collect();
        assert_eq!(
            places,
            [
                ("Meta-0".to_string(), 0, true),
                ("Meta-1".to_string(), 1920, false),
                ("Meta-2".to_string(), 3840, false)
            ]
        );
        assert_eq!(virtual_displays(9).len(), 4);
        assert_eq!(virtual_displays(0).len(), 1);
    }

    /// A stand-in for gst-launch-1.0: `script` runs in sh with the FIFO as $1.
    fn writer(script: &'static str) -> impl FnOnce(&std::path::Path) -> Command {
        move |fifo: &std::path::Path| {
            let mut command = Command::new("sh");
            command.args(["-c", script, "sh"]).arg(fifo);
            command
        }
    }

    #[test]
    fn the_pipeline_dies_with_the_thread_that_started_it() {
        // As a killed rtx-control's would (#2670): the starting thread ends without waiting.
        let pid = std::thread::spawn(|| {
            let mut sleep = Command::new("sleep");
            sleep.arg("30");
            die_with_parent(&mut sleep);
            let child = sleep.spawn().unwrap();
            let pid = child.id() as libc::pid_t;
            std::mem::forget(child);
            pid
        })
        .join()
        .unwrap();
        let mut status = 0;
        // SAFETY: waits for our own child.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL);
    }

    #[test]
    fn reads_the_negotiated_frame_size() {
        let out = "/GstPipeline:pipeline0/GstFileSink:filesink0.GstPad:sink: caps = video/x-raw, \
                   format=(string)BGRx, width=(int)1920, height=(int)1200, framerate=(fraction)0/1\n";
        assert_eq!(frame_size_from_caps(out), Some((1920, 1200)));
        assert_eq!(frame_size_from_caps("no caps here"), None);
    }

    #[test]
    fn only_a_missing_monitor_records_a_virtual_one() {
        // zbus's text for Mutter's answer on a computer with no monitor attached (#9).
        assert!(no_monitor("org.freedesktop.DBus.Error.Failed: Unknown monitor"));
        assert!(!no_monitor("org.freedesktop.DBus.Error.Failed: Session creation inhibited"));
        assert!(!no_monitor("org.freedesktop.DBus.Error.ServiceUnknown"));
    }

    #[test]
    fn a_virtual_monitors_stream_is_held_to_its_size_at_the_source() {
        let fifo = std::path::Path::new("/run/user/1000/frames.bgrx");
        let monitor = pipeline_args(42, None, fifo);
        assert_eq!(
            &monitor[..5],
            ["-v", "pipewiresrc", "path=42", "always-copy=true", "do-timestamp=true"]
        );
        assert!(!monitor.iter().any(|arg| arg.contains("width=")));
        assert_eq!(monitor.last().unwrap(), "sync=false");
        let virtual_ = pipeline_args(42, Some((1920, 1080)), fifo);
        // Right after pipewiresrc, before anything that could rescale.
        assert_eq!(&virtual_[5..7], ["!", "video/x-raw,width=1920,height=1080"]);
        assert_eq!(&virtual_[7..], &monitor[5..]);
        assert!(virtual_.contains(&"location=/run/user/1000/frames.bgrx".to_string()));
    }

    #[test]
    fn a_virtual_monitor_is_ready_once_its_first_frame_arrives_late() {
        // Caps and a 2x2 BGRx frame (16 bytes) only after a while, as a negotiating stream.
        let mut frames = FrameStream::spawn(writer(
            "sleep 0.6; echo '/GstPipeline:pipeline0/GstFileSink:filesink0.GstPad:sink: caps = \
             video/x-raw, format=(string)BGRx, width=(int)2, height=(int)2'; \
             printf '%016d' 0 > \"$1\"; sleep 5",
        ))
        .unwrap();
        let mut pokes = 0;
        let size = frames.wait_ready(Duration::from_secs(5), || pokes += 1).unwrap();
        assert_eq!(size, (2, 2));
        assert!(pokes >= 2, "nudged while waiting: {pokes}");
        assert_eq!(frames.seq(), 1);
    }

    #[test]
    fn a_virtual_monitor_whose_writer_exits_first_fails_at_once() {
        let mut frames = FrameStream::spawn(writer("exit 1")).unwrap();
        let started = Instant::now();
        let error = frames.wait_ready(Duration::from_secs(5), || {}).unwrap_err();
        assert!(error.to_string().contains("ended before its first frame"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_virtual_monitor_without_a_frame_gives_up_at_the_timeout() {
        let mut frames = FrameStream::spawn(writer("sleep 5")).unwrap();
        let error = frames
            .wait_ready(Duration::from_millis(600), || {})
            .unwrap_err();
        assert!(error.to_string().contains("no frame"), "{error}");
    }
}

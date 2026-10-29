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
//! records a virtual monitor instead, which Mutter sizes from the stream's negotiated format, as
//! gnome-remote-desktop does headless.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

use super::super::{Button, Shot};
use super::terminal;

const RD_BUS: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_BUS: &str = "org.gnome.Mutter.ScreenCast";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const CURSOR_EMBEDDED: u32 = 1;
/// The virtual monitor's size when no monitor is attached: Mutter makes it the size the stream
/// negotiates.
const VIRTUAL_SIZE: (u32, u32) = (1920, 1080);
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
    stream_path: OwnedObjectPath,
    node_id: u32,
    /// The stream's frame size: pointer coordinates are in these pixels, not the logical
    /// monitor size Mutter reports (they differ under fractional scaling).
    frame_size: Option<(u32, u32)>,
    /// What the clipboard serves, per MIME type, as last announced.
    clipboard_data: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    clipboard_enabled: bool,
    /// The clipboard as GNOME last announced it (#2682): changes, the offered MIME types, and
    /// whether this session owns it (its own paste).
    selection: Arc<Mutex<Selection>>,
    frames: Option<FrameStream>,
    /// The virtual monitor's size, when the session records one (no monitor attached).
    virtual_size: Option<(u32, u32)>,
    /// The last live-view frame: the stream frame it came from, the width it was made for.
    last_frame: Option<(u64, u32, super::super::Frame)>,
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
        // An empty connector records the primary monitor. With no monitor attached there is
        // none: record a virtual one, of the size the frame pipeline asks for (#9). As a
        // platform monitor, the desktop lays out on it as on a real one.
        let mut virtual_size = None;
        let stream_path: OwnedObjectPath = match sc_session.call("RecordMonitor", &("", record()))
        {
            Ok(path) => path,
            Err(error) if no_monitor(&error.to_string()) => {
                let mut properties = record();
                properties.insert("is-platform", Value::Bool(true));
                virtual_size = Some(VIRTUAL_SIZE);
                sc_session
                    .call("RecordVirtual", &(properties,))
                    .context("no monitor attached, and Mutter recorded no virtual one")?
            }
            Err(error) => return Err(error.into()),
        };
        let stream = Proxy::new_owned(conn.clone(), SC_BUS, stream_path.clone(), SC_STREAM)?;

        let (tx, rx) = mpsc::channel();
        let mut added = stream.receive_signal("PipeWireStreamAdded")?;
        std::thread::spawn(move || {
            if let Some(message) = added.next() {
                let _ = tx.send(message.body().deserialize::<u32>());
            }
        });
        session.call::<_, _, ()>("Start", &())?;
        let node_id = rx
            .recv_timeout(STREAM_TIMEOUT)
            .map_err(|_| anyhow!("GNOME did not start the screen stream"))??;

        let mut input = Self {
            session,
            stream_path,
            node_id,
            frame_size: None,
            clipboard_data: Arc::new(Mutex::new(HashMap::new())),
            clipboard_enabled: false,
            selection: Arc::new(Mutex::new(Selection::default())),
            frames: None,
            virtual_size,
            last_frame: None,
        };
        // A virtual monitor exists only once its stream is negotiated and read: read it from the
        // start and wait for its first frame, so the first input (before any screenshot) has a
        // monitor to land on and is scaled to its size. A failure here drops `input`, which
        // stops the session.
        if let Some(size) = virtual_size {
            let mut frames = FrameStream::start(node_id, Some(size))?;
            let (width, height) = frames.wait_ready(FRAME_TIMEOUT, || {
                let _ = input.nudge();
            })?;
            input.frame_size = Some((width as u32, height as u32));
            input.frames = Some(frames);
        }
        Ok(input)
    }

    /// Frame pixels; known after the first screenshot.
    pub fn screen_size(&self) -> Option<(f64, f64)> {
        self.frame_size.map(|(w, h)| (w as f64, h as f64))
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

    /// The latest frame of the session's stream, one newer than this request when the screen
    /// is changing (an action's effect shows), or else the latest. A `live` view does not
    /// wait: it nudges for the next frame and takes what is there, at most one frame behind.
    pub fn screenshot(&mut self, max_width: u32, live: bool) -> Result<Shot> {
        let (data, width, height) = self.latest_frame(live, false)?;
        super::x11::shot(
            &data[..width * height * 4],
            width,
            height,
            max_width,
            "display".into(),
        )
    }

    /// The latest picture of the stream as a live-view video frame: no waiting for a newer one
    /// (the stream sends a frame when the screen changes).
    /// Until the stream sends a newer frame, the last one comes back as it was: not copied or
    /// scaled again, and with the same number, so the encoder skips it (#2683). The live view asks
    /// 30 times a second, and GNOME sends a frame only when the screen changes.
    pub fn frame(&mut self, max_width: u32) -> Result<super::super::Frame> {
        if let Some((seq, width, frame)) = &self.last_frame {
            let unchanged = self
                .frames
                .as_mut()
                .is_some_and(|f| f.running() && f.seq() == *seq);
            if unchanged && *width == max_width {
                return Ok(frame.clone());
            }
        }
        let (data, width, height, stream_seq) = self.numbered_frame(false, true)?;
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
        self.last_frame = Some((stream_seq, max_width, frame.clone()));
        Ok(frame)
    }

    /// BGRA of the latest stream frame. The first one, a `live` view's and a fresh screenshot's
    /// wait for a newer frame as before; a video frame (`frame`) takes what is there.
    fn latest_frame(&mut self, live: bool, video: bool) -> Result<(Arc<Vec<u8>>, usize, usize)> {
        let (data, width, height, _) = self.numbered_frame(live, video)?;
        Ok((data, width, height))
    }

    /// [`Self::latest_frame`] with the stream's number for it.
    fn numbered_frame(
        &mut self,
        live: bool,
        video: bool,
    ) -> Result<(Arc<Vec<u8>>, usize, usize, u64)> {
        if !self.frames.as_mut().is_some_and(FrameStream::running) {
            self.frames = None;
            // A new stream numbers its frames from 1 again: nothing cached may match them.
            self.last_frame = None;
            self.frames = Some(FrameStream::start(self.node_id, self.virtual_size)?);
        }
        let frames = self.frames.as_ref().unwrap();
        let before = frames.seq();
        if before == 0 {
            self.nudge()?;
            if !frames.wait_newer(0, FRAME_TIMEOUT) {
                self.frames = None;
                bail!("no frame from the screen stream in {FRAME_TIMEOUT:?}");
            }
        } else if live {
            self.nudge()?;
            frames.wait_newer(before, LIVE_FRAME_WAIT);
        } else if !video && !frames.wait_newer(before, FRESH_FRAME_WAIT) {
            self.nudge()?;
            frames.wait_newer(before, FRESH_FRAME_WAIT);
        }
        let frames = self.frames.as_ref().unwrap();
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
        self.frame_size = Some((width as u32, height as u32));
        Ok((data, width, height, seq))
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.session.call::<_, _, ()>(
            "NotifyPointerMotionAbsolute",
            &(self.stream_path.as_str(), x, y),
        )?;
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
        self.frames = None;
        let _ = self.session.call::<_, _, ()>("Stop", &());
    }
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
    use super::{die_with_parent, frame_size_from_caps, no_monitor, pipeline_args, FrameStream};
    use std::process::Command;
    use std::time::{Duration, Instant};

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

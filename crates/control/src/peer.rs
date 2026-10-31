//! Remote Control's live view as a WebRTC peer (`rtx-control`, ops `peer_*`).
//!
//! The viewer's offer arrives through the server (over Tailscale or the Public Exposure relay,
//! whichever reached this computer); the answer goes back the same way. After that the screen
//! (VP8) and the DataChannel travel directly between the two machines. This module is only the
//! transport: frames come from the control loop's capture (sent as VP8, which is royalty-free), and
//! DataChannel text goes to the server,
//! which checks the client's permission and runs the actions as for `/actions`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use shiguredo_libvpx::{
    CodecConfig, EncodeOptions, Encoder, EncoderConfig, EncodingDeadline, ImageData, ImageFormat,
    RateControlMode, Vp8Config,
};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::crypto::Sha1HmacProvider;
use str0m::format::Codec;
use str0m::ice::{StunMessage, TransId};
use str0m::media::{Frequency, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::platform::Frame;

/// A public STUN server, to learn this computer's address as seen from the internet (the
/// server-reflexive candidate) when the viewer is on another network.
const STUN_SERVER: &str = "stun.l.google.com:19302";
const STUN_TIMEOUT: Duration = Duration::from_millis(800);
/// Frames a second, at most, on every platform: enough for remote control, and a fully moving
/// screen costs the host less than at 30 (#2685). A still screen sends only real changes. Every
/// request is held to it, asked for or not; GNOME Wayland's stream has the same cap
/// (wayland_input.rs MAX_FRAME_RATE).
pub const MAX_FPS: u32 = 15;
const DEFAULT_FPS: u32 = MAX_FPS;
const DEFAULT_MAX_WIDTH: u32 = 1280;
const DEFAULT_BITRATE: u32 = 2_500_000;
/// A session that never connects, or loses its viewer, ends after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// DataChannel messages waiting for room in the send buffer, at most (a few pulled chunks).
const MAX_OUTBOX_BYTES: usize = 16 * 1024 * 1024;
/// How soon a waiting message is tried again.
const OUTBOX_RETRY: Duration = Duration::from_millis(5);
/// Interfaces that never reach a viewer: container and VM bridges.
const SKIPPED_INTERFACES: &[&str] = &["docker", "br-", "veth", "lxc", "virbr", "vmnet", "utun"];

/// What a session asks of the control loop, which owns capture and stdout.
pub enum ToMain {
    /// Capture a frame of `display` (None: the primary) at most `max_width` wide and send it back
    /// on `reply`.
    Frame {
        max_width: u32,
        display: Option<String>,
        reply: Sender<SessionIn>,
    },
    /// The display this view shows went away (realtimex-link#12): said once, until it is given
    /// another. The view keeps its last picture meanwhile; rtx-linkd picks the next display.
    DisplayLost { session: u64, display: String },
    /// Text from the viewer's DataChannel, for the server.
    Message { session: u64, text: String },
    /// `connected`, or `closed` (the session is over).
    State { session: u64, state: &'static str },
}

pub enum SessionIn {
    Net {
        source: SocketAddr,
        destination: SocketAddr,
        data: Vec<u8>,
    },
    Frame(Result<Frame>),
    Send(String),
    /// Show this display from the next frame on (None: the primary).
    Display(Option<String>),
    Close,
}

/// The control loop's handle on one session.
pub struct Peer {
    tx: Sender<SessionIn>,
}

impl Peer {
    pub fn send(&self, text: String) {
        let _ = self.tx.send(SessionIn::Send(text));
    }

    /// Show `display` (None: the primary) from the next frame on, starting with a keyframe.
    pub fn show(&self, display: Option<String>) {
        let _ = self.tx.send(SessionIn::Display(display));
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.tx.send(SessionIn::Close);
    }
}

pub struct Options {
    /// The display it shows (None: the primary); `peer_display` changes it (realtimex-link#12).
    pub display: Option<String>,
    pub max_width: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Ask a STUN server for this computer's public address (off in tests).
    pub stun: bool,
}

impl Options {
    /// The frame rate in effect: what was asked, between 1 and MAX_FPS.
    fn frame_rate(&self) -> u32 {
        self.fps.clamp(1, MAX_FPS)
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            display: None,
            max_width: DEFAULT_MAX_WIDTH,
            fps: DEFAULT_FPS,
            bitrate: DEFAULT_BITRATE,
            stun: true,
        }
    }
}

/// Answer `offer` and run the session on its own thread. Returns the handle and the answer SDP.
pub fn start(
    session: u64,
    offer: &str,
    options: Options,
    to_main: Sender<ToMain>,
) -> Result<(Peer, String)> {
    let remote_password = ice_password(offer);
    let offer = SdpOffer::from_sdp_string(&without_mdns_candidates(offer))
        .map_err(|e| anyhow!("bad offer: {e}"))?;
    let crypto = str0m::crypto::from_feature_flags();
    let sha1_hmac = crypto.sha1_hmac_provider;
    let mut rtc = RtcConfig::new()
        .set_crypto_provider(Arc::new(crypto))
        .clear_codecs()
        .enable_vp8(true)
        .build(Instant::now());

    let (tx, rx) = mpsc::channel();
    // Ends the socket readers when this session ends, or when it fails before it starts.
    let readers = ReadersOpen::default();
    let mut sockets = HashMap::new();
    for ip in local_addresses() {
        let socket = match UdpSocket::bind(SocketAddr::new(ip, 0)) {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        let local = socket.local_addr()?;
        if let Ok(candidate) = Candidate::host(local, "udp") {
            rtc.add_local_candidate(candidate);
        }
        if options.stun && is_private_lan(ip) {
            if let Some(public) = stun_mapped_address(&socket) {
                if let Ok(candidate) = Candidate::server_reflexive(public, local, "udp") {
                    rtc.add_local_candidate(candidate);
                }
            }
        }
        spawn_reader(socket.try_clone()?, tx.clone(), readers.0.clone());
        sockets.insert(local, socket);
    }
    if sockets.is_empty() {
        bail!("no network interface to reach the viewer on");
    }

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| anyhow!("offer not accepted: {e}"))?
        .to_sdp_string();
    let nominations = remote_password.map(|remote| {
        let local = rtc.direct_api().local_ice_credentials().pass;
        NominationGate::new(local, remote, sha1_hmac)
    });

    let peer = Peer { tx: tx.clone() };
    thread::Builder::new()
        .name(format!("peer-{session}"))
        .spawn(move || {
            let _readers = readers;
            let mut runner = Session {
                id: session,
                rtc,
                sockets,
                nominations,
                to_main: to_main.clone(),
                me: tx,
                options,
                video: None,
                channel: None,
                encoder: None,
                force_keyframe: false,
                display_lost: false,
                stale_frame: false,
                last_frame: None,
                frame_pending: false,
                next_frame: Instant::now(),
                started: Instant::now(),
                connected: false,
                outbox: std::collections::VecDeque::new(),
                outbox_bytes: 0,
            };
            if let Err(error) = runner.run(rx) {
                eprintln!("[peer {session}] ended: {error:#}");
            }
            let _ = to_main.send(ToMain::State {
                session,
                state: "closed",
            });
        })?;
    Ok((peer, answer))
}

struct Session {
    id: u64,
    rtc: Rtc,
    sockets: HashMap<SocketAddr, UdpSocket>,
    /// None when the offer had no ICE password (str0m then refuses its checks anyway).
    nominations: Option<NominationGate>,
    to_main: Sender<ToMain>,
    me: Sender<SessionIn>,
    options: Options,
    /// The video m-line and its VP8 payload type, once the media is known.
    video: Option<(Mid, str0m::format::PayloadParams)>,
    channel: Option<ChannelId>,
    /// The VP8 encoder and the frame size and bitrate it was made for (a change needs a new one).
    encoder: Option<(Encoder, (usize, usize, u32))>,
    force_keyframe: bool,
    /// Its display went away and that was said (`ToMain::DisplayLost`, once per display).
    display_lost: bool,
    /// The frame on its way was asked of the display shown before a switch: it is dropped.
    stale_frame: bool,
    /// The `seq` and size of the last frame encoded.
    last_frame: Option<(u64, usize, usize)>,
    frame_pending: bool,
    next_frame: Instant,
    started: Instant,
    connected: bool,
    /// DataChannel messages waiting for room in the SCTP send buffer: str0m refuses a write
    /// that does not fit, and a large reply (a pulled chunk, #2682) must not be lost.
    outbox: std::collections::VecDeque<Vec<u8>>,
    outbox_bytes: usize,
}

impl Session {
    fn run(&mut self, rx: Receiver<SessionIn>) -> Result<()> {
        loop {
            let deadline = match self.rtc.poll_output()? {
                Output::Timeout(at) => at,
                Output::Transmit(t) => {
                    if let Some(socket) = self.sockets.get(&t.source) {
                        if let Some(gate) = &mut self.nominations {
                            gate.sent(t.source, t.destination, &t.contents);
                        }
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                    continue;
                }
                Output::Event(event) => {
                    if !self.on_event(event)? {
                        return Ok(());
                    }
                    continue;
                }
            };
            if self.flush_outbox() {
                continue;
            }
            let now = Instant::now();
            if !self.connected && now - self.started > CONNECT_TIMEOUT {
                bail!("the viewer did not connect in {CONNECT_TIMEOUT:?}");
            }
            self.request_frame(now);
            let mut wait = deadline.saturating_duration_since(now);
            if self.wants_frame() {
                wait = wait.min(self.next_frame.saturating_duration_since(now));
            }
            if !self.outbox.is_empty() {
                wait = wait.min(OUTBOX_RETRY);
            }
            let input = match rx.recv_timeout(wait.max(Duration::from_millis(1))) {
                Ok(input) => input,
                Err(RecvTimeoutError::Timeout) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now()))?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            match input {
                SessionIn::Net {
                    source,
                    destination,
                    data,
                } => {
                    let data = match &mut self.nominations {
                        Some(gate) => match gate.received(source, destination, &data) {
                            Some(data) => data,
                            None => continue,
                        },
                        None => Cow::Borrowed(data.as_slice()),
                    };
                    let Ok(contents) = data.as_ref().try_into() else {
                        continue;
                    };
                    let receive = Receive {
                        proto: Protocol::Udp,
                        source,
                        destination,
                        contents,
                    };
                    self.rtc
                        .handle_input(Input::Receive(Instant::now(), receive))?;
                }
                SessionIn::Frame(frame) => {
                    self.frame_pending = false;
                    if std::mem::take(&mut self.stale_frame) {
                        continue;
                    }
                    match frame {
                        Ok(frame) => {
                            self.display_lost = false;
                            self.send_frame(frame)?
                        }
                        // The screen may be locked; try again at the next interval.
                        Err(error) => {
                            if let Some(display) =
                                lost_display(&self.options.display, &mut self.display_lost, &error)
                            {
                                let _ = self.to_main.send(ToMain::DisplayLost {
                                    session: self.id,
                                    display,
                                });
                            } else if !self.display_lost {
                                eprintln!("[peer {}] no frame: {error:#}", self.id);
                            }
                        }
                    }
                }
                SessionIn::Display(display) => {
                    if display != self.options.display {
                        self.options.display = display;
                        self.display_lost = false;
                        // The next frame is another picture, maybe of another size: a keyframe,
                        // and a new encoder when the size changed (send_frame).
                        self.force_keyframe = true;
                        self.last_frame = None;
                        self.stale_frame = self.frame_pending;
                    }
                }
                SessionIn::Send(text) => {
                    if self.outbox_bytes + text.len() > MAX_OUTBOX_BYTES {
                        eprintln!(
                            "[peer {}] DataChannel backlog full: a message was dropped",
                            self.id
                        );
                    } else {
                        self.outbox_bytes += text.len();
                        self.outbox.push_back(text.into_bytes());
                    }
                }
                SessionIn::Close => {
                    self.rtc.disconnect();
                    return Ok(());
                }
            }
        }
    }

    /// Write queued DataChannel messages while the send buffer has room, in order. True when
    /// something was written (so str0m has output to send).
    fn flush_outbox(&mut self) -> bool {
        let Some(mut channel) = self.channel.and_then(|id| self.rtc.channel(id)) else {
            return false;
        };
        let mut wrote = false;
        while let Some(message) = self.outbox.front() {
            match channel.write(false, message) {
                Ok(true) => {
                    self.outbox_bytes -= message.len();
                    self.outbox.pop_front();
                    wrote = true;
                }
                // No room yet: try again once some of it is sent.
                Ok(false) => break,
                Err(error) => {
                    eprintln!("[peer {}] DataChannel write failed: {error}", self.id);
                    self.outbox_bytes -= message.len();
                    self.outbox.pop_front();
                }
            }
        }
        wrote
    }

    /// Handle one event; false ends the session.
    fn on_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::IceConnectionStateChange(IceConnectionState::Connected) if !self.connected => {
                self.connected = true;
                let _ = self.to_main.send(ToMain::State {
                    session: self.id,
                    state: "connected",
                });
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return Ok(false),
            Event::MediaAdded(media) => {
                let params = self.rtc.writer(media.mid).and_then(|writer| {
                    writer
                        .payload_params()
                        .find(|p| p.spec().codec == Codec::Vp8)
                        .cloned()
                });
                if let Some(params) = params {
                    self.video = Some((media.mid, params));
                }
            }
            Event::KeyframeRequest(_) => self.force_keyframe = true,
            Event::ChannelOpen(id, _label) => self.channel = Some(id),
            Event::ChannelData(data) => {
                if let Ok(text) = String::from_utf8(data.data) {
                    // The viewer's picture quality is the helper's own business (#2681).
                    if let Some(quality) = Quality::from_message(&text) {
                        self.options.max_width = quality.max_width();
                        self.options.bitrate = quality.bitrate();
                        self.force_keyframe = true;
                        return Ok(true);
                    }
                    let _ = self.to_main.send(ToMain::Message {
                        session: self.id,
                        text,
                    });
                }
            }
            Event::ChannelClose(_) => return Ok(false),
            _ => {}
        }
        Ok(true)
    }

    fn wants_frame(&self) -> bool {
        self.connected && self.video.is_some() && !self.frame_pending
    }

    fn request_frame(&mut self, now: Instant) {
        if !self.wants_frame() || now < self.next_frame {
            return;
        }
        self.frame_pending = true;
        self.next_frame = now + Duration::from_secs(1) / self.options.frame_rate();
        let _ = self.to_main.send(ToMain::Frame {
            max_width: self.options.max_width,
            display: self.options.display.clone(),
            reply: self.me.clone(),
        });
    }

    fn send_frame(&mut self, frame: Frame) -> Result<()> {
        let Some((mid, params)) = self.video else {
            return Ok(());
        };
        // Nothing on screen changed: the viewer keeps showing the last frame (#2683).
        let (width, height) = (frame.width as usize, frame.height as usize);
        if !self.force_keyframe && is_repeat(self.last_frame, &frame) {
            return Ok(());
        }
        self.last_frame = Some((frame.seq, width, height));
        let bitrate = self.options.bitrate;
        if self.encoder.as_ref().map(|(_, made_for)| *made_for) != Some((width, height, bitrate)) {
            let mut config = EncoderConfig::new(
                width,
                height,
                ImageFormat::I420,
                // Screen content: skip blocks that did not change (most of a desktop, most of the time).
                CodecConfig::Vp8(Vp8Config {
                    static_threshold: Some(1000),
                    noise_sensitivity: Some(0),
                    ..Vp8Config::default()
                }),
            );
            config.fps_numerator = self.options.frame_rate() as usize;
            config.target_bitrate = bitrate as usize;
            config.deadline = EncodingDeadline::Realtime;
            // Variable bitrate up to the target: a still screen costs little, a busy one gets the rest.
            config.rate_control = RateControlMode::Vbr;
            config.cpu_used = Some(8);
            config.error_resilient = true;
            config.max_quantizer = 56;
            let encoder = Encoder::new(config).map_err(|e| anyhow!("VP8 encoder: {e}"))?;
            self.encoder = Some((encoder, (width, height, bitrate)));
            self.force_keyframe = true;
        }
        let (y, u, v) = bgra_to_i420(&frame.data, width, height);
        let options = EncodeOptions {
            force_keyframe: std::mem::take(&mut self.force_keyframe),
        };
        let (encoder, _) = self.encoder.as_mut().unwrap();
        encoder
            .encode(
                &ImageData::I420 {
                    y: &y,
                    u: &u,
                    v: &v,
                },
                &options,
            )
            .map_err(|e| anyhow!("VP8 encode: {e}"))?;
        let mut data = Vec::new();
        while let Some(packet) = encoder.next_frame() {
            data.extend_from_slice(packet.data());
        }
        if data.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        let ticks = (now - self.started).as_micros() as u64 * 90 / 1000;
        if let Some(writer) = self.rtc.writer(mid) {
            writer.write(
                params.pt(),
                now,
                MediaTime::new(ticks, Frequency::NINETY_KHZ),
                data,
            )?;
        }
        Ok(())
    }
}

/// The display to report gone, when a frame failed because `display` (the one this view shows)
/// isn't attached and that wasn't said yet: once, so a view isn't flooded at its frame rate.
fn lost_display(
    display: &Option<String>,
    reported: &mut bool,
    error: &anyhow::Error,
) -> Option<String> {
    let display = display.as_ref()?;
    if *reported || !crate::displays::not_found(error) {
        return None;
    }
    *reported = true;
    Some(display.clone())
}

/// `frame` is the picture last encoded: numbered, the same number and the same size. A different
/// size always counts as new, so the encoder is rebuilt for it.
fn is_repeat(last: Option<(u64, usize, usize)>, frame: &Frame) -> bool {
    frame.seq != 0 && last == Some((frame.seq, frame.width as usize, frame.height as usize))
}

/// A picture quality the viewer picks (#2681), by hand or with Auto (route and network): the
/// widest picture sent and the most bits a second. The frame rate stays MAX_FPS for all three.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Quality {
    High,
    Balanced,
    Saver,
}

impl Quality {
    /// `{"type":"quality","level":"high"|"balanced"|"saver"}` on the DataChannel.
    fn from_message(text: &str) -> Option<Self> {
        let message: serde_json::Value = serde_json::from_str(text).ok()?;
        if message.get("type")?.as_str()? != "quality" {
            return None;
        }
        match message.get("level")?.as_str()? {
            "high" => Some(Self::High),
            "balanced" => Some(Self::Balanced),
            "saver" => Some(Self::Saver),
            _ => None,
        }
    }

    fn max_width(self) -> u32 {
        match self {
            Self::High => 2560,
            Self::Balanced => 1920,
            Self::Saver => 1280,
        }
    }

    fn bitrate(self) -> u32 {
        match self {
            Self::High => 8_000_000,
            Self::Balanced => 3_000_000,
            Self::Saver => 1_200_000,
        }
    }
}

/// Packed BGRA (even sides) to planar I420, BT.601 limited range, chroma averaged over 2x2 pixels.
fn bgra_to_i420(bgra: &[u8], width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (cw, ch) = (width / 2, height / 2);
    let mut y = vec![0u8; width * height];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    let luma = |b: i32, g: i32, r: i32| ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
    for row in 0..height {
        for col in 0..width {
            let p = &bgra[(row * width + col) * 4..];
            y[row * width + col] = luma(p[0] as i32, p[1] as i32, p[2] as i32) as u8;
        }
    }
    for row in 0..ch {
        for col in 0..cw {
            let (mut b, mut g, mut r) = (0i32, 0i32, 0i32);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let p = &bgra[((row * 2 + dy) * width + col * 2 + dx) * 4..];
                b += p[0] as i32;
                g += p[1] as i32;
                r += p[2] as i32;
            }
            let (b, g, r) = (b / 4, g / 4, r / 4);
            u[row * cw + col] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            v[row * cw + col] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
    (y, u, v)
}

/// How often a socket reader looks up from `recv_from` to see whether its session ended.
const READER_POLL: Duration = Duration::from_millis(250);

/// True while a session's socket readers should run; dropping it stops them within READER_POLL.
/// A reader holds a clone of its socket, so the session closing its own copy does not wake it.
struct ReadersOpen(Arc<AtomicBool>);

impl Default for ReadersOpen {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}

impl Drop for ReadersOpen {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Every packet a socket receives goes to the session thread, until `open` turns false.
fn spawn_reader(
    socket: UdpSocket,
    tx: Sender<SessionIn>,
    open: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Ok(local) = socket.local_addr() else {
            return;
        };
        if socket.set_read_timeout(Some(READER_POLL)).is_err() {
            return;
        }
        let mut buf = vec![0u8; 2000];
        while open.load(Ordering::Relaxed) {
            let (n, source) = match socket.recv_from(&mut buf) {
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(_) => return,
            };
            if misrouted_tailscale(source, local) {
                continue;
            }
            let packet = SessionIn::Net {
                source,
                destination: local,
                data: buf[..n].to_vec(),
            };
            if tx.send(packet).is_err() {
                return;
            }
        }
    })
}

/// Addresses a viewer may reach: IPv4 (the LAN, and the Tailscale address when there is one) and
/// public IPv6. With IPv6 there is no NAT, so two networks that drop each other's IPv4 UDP (two
/// Viettel subscribers, #2657) can often still connect directly.
fn local_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter(|iface| {
            !SKIPPED_INTERFACES
                .iter()
                .any(|prefix| iface.name.starts_with(prefix))
        })
        .map(|iface| iface.ip())
        .filter(|ip| is_reachable_candidate(*ip))
        .collect()
}

/// IPv4 except link-local; IPv6 only when globally routable (2000::/3), so never link-local
/// (fe80::/10, which needs a scope) or unique-local (fc00::/7, e.g. a router's private prefix).
fn is_reachable_candidate(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xe000 == 0x2000,
    }
}

/// A LAN address behind a NAT (not Tailscale's 100.64.0.0/10): worth asking STUN about.
fn is_private_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(_) => false,
    }
}

/// Tailscale's addresses: 100.64.0.0/10 (IPv4) and fd7a:115c:a1e0::/48 (IPv6).
fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (b & 0xc0) == 64
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// A packet from a Tailscale address that arrived on a non-Tailscale socket (#2674). A viewer on
/// Tailscale sends ICE checks from its Tailscale socket too, and Linux puts them on the LAN with
/// that source. Answering would teach the peer an address this computer cannot reach (it is not on
/// Tailscale, or the reply leaves by another way), and DTLS would go there and time out. Real
/// Tailscale traffic arrives on the Tailscale address, so nothing that works is lost.
fn misrouted_tailscale(source: SocketAddr, local: SocketAddr) -> bool {
    is_tailscale(source.ip()) && !is_tailscale(local.ip())
}

/// Our ICE checks remembered at once, to match their answers; the oldest is forgotten first.
const OUTSTANDING_CHECKS: usize = 256;

/// Holds a viewer's nomination of a path back until this computer has seen the path work both
/// ways (#2674).
///
/// A viewer with two networks (a USB tether and Wi-Fi, or Tailscale) sends ICE checks from one
/// network's address over the other. They reach us, but our answers to that address cannot reach
/// the viewer. If the viewer nominates (USE-CANDIDATE) such a pair, str0m sends DTLS there and the
/// view times out. So a nomination counts only on a pair where one of our own checks was answered:
/// a binding success response with the transaction ID of a check we sent to that address from
/// that socket, signed with the viewer's ICE password. Until then the nomination reaches str0m
/// without USE-CANDIDATE, re-signed with our password, as an ordinary check: str0m still learns a
/// new (peer-reflexive) address from it and checks it back. The viewer repeats its nomination and
/// it is taken once the path is proven.
struct NominationGate {
    local_password: String,
    remote_password: String,
    sha1_hmac: &'static dyn Sha1HmacProvider,
    /// Our checks waiting for an answer: transaction, our socket, the address checked.
    outstanding: VecDeque<(TransId, SocketAddr, SocketAddr)>,
    /// (our socket, viewer address) pairs where one of our checks was answered.
    proven: HashSet<(SocketAddr, SocketAddr)>,
}

impl NominationGate {
    fn new(
        local_password: String,
        remote_password: String,
        sha1_hmac: &'static dyn Sha1HmacProvider,
    ) -> Self {
        Self {
            local_password,
            remote_password,
            sha1_hmac,
            outstanding: VecDeque::new(),
            proven: HashSet::new(),
        }
    }

    /// Remember a check str0m sends, to recognise its answer.
    fn sent(&mut self, source: SocketAddr, destination: SocketAddr, packet: &[u8]) {
        let Ok(message) = StunMessage::parse(packet) else {
            return;
        };
        if !message.is_binding_request() {
            return;
        }
        if self.outstanding.len() == OUTSTANDING_CHECKS {
            self.outstanding.pop_front();
        }
        self.outstanding
            .push_back((message.trans_id(), source, destination));
    }

    /// The packet for str0m: as received, or an unproven nomination without USE-CANDIDATE.
    /// None drops it.
    fn received<'a>(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        packet: &'a [u8],
    ) -> Option<Cow<'a, [u8]>> {
        let Ok(message) = StunMessage::parse(packet) else {
            return Some(Cow::Borrowed(packet));
        };
        let sha1_hmac = |key: &[u8], payloads: &[&[u8]]| self.sha1_hmac.sha1_hmac(key, payloads);
        if message.is_successful_binding_response() {
            let ours = self.outstanding.iter().position(|(id, from, to)| {
                *id == message.trans_id() && *from == destination && *to == source
            });
            if let Some(at) = ours {
                if message.verify(self.remote_password.as_bytes(), sha1_hmac) {
                    self.outstanding.remove(at);
                    self.proven.insert((destination, source));
                }
            }
            return Some(Cow::Borrowed(packet));
        }
        if !message.is_binding_request()
            || !message.use_candidate()
            || self.proven.contains(&(destination, source))
            // str0m drops a check it cannot verify; let it.
            || !message.verify(self.local_password.as_bytes(), sha1_hmac)
        {
            return Some(Cow::Borrowed(packet));
        }
        let (controlling, tie_breaker) = match (message.ice_controlling(), message.ice_controlled())
        {
            (Some(tie_breaker), _) => (true, tie_breaker),
            (None, Some(tie_breaker)) => (false, tie_breaker),
            (None, None) => return None,
        };
        let check = StunMessage::binding_request(
            message.username()?,
            message.trans_id(),
            controlling,
            tie_breaker,
            message.prio()?,
            false,
        );
        let mut buf = vec![0u8; 1500];
        let n = check
            .to_bytes(Some(self.local_password.as_bytes()), &mut buf, sha1_hmac)
            .ok()?;
        buf.truncate(n);
        Some(Cow::Owned(buf))
    }
}

/// The `a=ice-pwd` of an SDP (the session's, or the first media section's).
fn ice_password(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|line| line.trim_end().strip_prefix("a=ice-pwd:"))
        .map(str::to_string)
}

/// Browsers hide their host addresses behind mDNS names (`<uuid>.local`) that we cannot resolve.
/// Drop those candidates: the browser's checks still reach us, and it becomes peer-reflexive.
fn without_mdns_candidates(sdp: &str) -> String {
    sdp.split_inclusive('\n')
        .filter(|line| !(line.starts_with("a=candidate:") && line.contains(".local ")))
        .collect()
}

/// This socket's address as the STUN server sees it (RFC 5389 binding request).
fn stun_mapped_address(socket: &UdpSocket) -> Option<SocketAddr> {
    let server = STUN_SERVER
        .to_socket_addrs()
        .ok()?
        .find(|addr| addr.is_ipv4())?;
    let mut request = [0u8; 20];
    request[..2].copy_from_slice(&0x0001u16.to_be_bytes());
    request[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
    let id: [u8; 12] = std::array::from_fn(|i| (nanos_seed() >> (i % 8 * 8)) as u8 ^ i as u8);
    request[8..].copy_from_slice(&id);
    socket.set_read_timeout(Some(STUN_TIMEOUT)).ok()?;
    socket.send_to(&request, server).ok()?;
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + STUN_TIMEOUT;
    let result = loop {
        if Instant::now() > deadline {
            break None;
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == server => break parse_binding_response(&buf[..n], &id),
            Ok(_) => continue,
            Err(_) => break None,
        }
    };
    socket.set_read_timeout(None).ok()?;
    result
}

fn nanos_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5eed)
}

/// XOR-MAPPED-ADDRESS (IPv4) of a binding success response for transaction `id`.
fn parse_binding_response(msg: &[u8], id: &[u8; 12]) -> Option<SocketAddr> {
    if msg.len() < 20 || msg[0..2] != [0x01, 0x01] || &msg[8..20] != id {
        return None;
    }
    let length = u16::from_be_bytes([msg[2], msg[3]]) as usize;
    let attrs = msg.get(20..20 + length)?;
    let mut at = 0;
    while at + 4 <= attrs.len() {
        let kind = u16::from_be_bytes([attrs[at], attrs[at + 1]]);
        let len = u16::from_be_bytes([attrs[at + 2], attrs[at + 3]]) as usize;
        let value = attrs.get(at + 4..at + 4 + len)?;
        if kind == 0x0020 && len >= 8 && value[1] == 0x01 {
            let port = u16::from_be_bytes([value[2], value[3]]) ^ 0x2112;
            let ip = [
                value[4] ^ 0x21,
                value[5] ^ 0x12,
                value[6] ^ 0xA4,
                value[7] ^ 0x42,
            ];
            return Some(SocketAddr::from((ip, port)));
        }
        at += 4 + len.div_ceil(4) * 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_readers_end_with_their_session_without_another_packet() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (tx, _rx) = mpsc::channel();
        let readers = ReadersOpen::default();
        let reader = spawn_reader(socket.try_clone().unwrap(), tx, readers.0.clone());
        drop(socket);
        drop(readers);
        let started = Instant::now();
        while !reader.is_finished() && started.elapsed() < READER_POLL * 8 {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            reader.is_finished(),
            "the reader kept its socket after the session ended"
        );
    }

    #[test]
    fn bgra_to_i420_maps_white_black_and_red() {
        // 2x2 frame: white, black / red, red (BGRA).
        let bgra = [
            255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ];
        let (y, u, v) = bgra_to_i420(&bgra, 2, 2);
        assert_eq!(y, vec![235, 16, 82, 82]);
        assert_eq!(u.len(), 1);
        assert_eq!(v.len(), 1);
        // Mostly red: V well above neutral, U below it.
        assert!(v[0] > 160 && u[0] < 128);
    }

    #[test]
    fn mdns_candidates_are_dropped_and_the_rest_kept() {
        let sdp = "v=0\r\na=candidate:1 1 udp 2122260223 4f2b-1.local 54321 typ host\r\n\
                   a=candidate:2 1 udp 1686052607 27.74.244.61 54321 typ srflx raddr 0.0.0.0 rport 0\r\n";
        let kept = without_mdns_candidates(sdp);
        assert!(!kept.contains(".local"));
        assert!(kept.contains("typ srflx"));
        assert!(kept.starts_with("v=0\r\n"));
    }

    #[test]
    fn xor_mapped_address_is_decoded() {
        let id = [7u8; 12];
        let mut msg = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xA4, 0x42];
        msg.extend_from_slice(&id);
        // XOR-MAPPED-ADDRESS, IPv4 203.0.113.9:40000
        msg.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        msg.extend_from_slice(&(40000u16 ^ 0x2112).to_be_bytes());
        // 203.0.113.9 XOR the magic cookie 0x2112A442.
        msg.extend_from_slice(&[0xEA, 0x12, 0xD5, 0x4B]);
        assert_eq!(
            parse_binding_response(&msg, &id),
            Some("203.0.113.9:40000".parse().unwrap())
        );
        assert_eq!(parse_binding_response(&msg, &[8u8; 12]), None);
    }

    #[test]
    fn offers_ipv4_and_public_ipv6_only() {
        let ok = |s: &str| is_reachable_candidate(s.parse().unwrap());
        assert!(ok("192.168.1.14"));
        assert!(ok("100.125.230.40"));
        assert!(!ok("169.254.10.1"));
        // A Viettel home line's public IPv6 (#2657).
        assert!(ok("2402:800:631d:b86:2d42:2887:7c2:811b"));
        assert!(!ok("fe80::1"));
        assert!(!ok("fc42:5009:ba4b:5ab0::1"));
        assert!(!ok("fd7a:115c:a1e0::1"));
    }

    #[test]
    fn tailscale_packets_on_a_lan_socket_are_ignored() {
        let lan: SocketAddr = "192.168.11.73:50000".parse().unwrap();
        let tailnet: SocketAddr = "100.90.177.124:50000".parse().unwrap();
        let viewer_tailscale: SocketAddr = "100.125.230.40:53943".parse().unwrap();
        let viewer_lan: SocketAddr = "192.168.11.67:60700".parse().unwrap();
        // The misrouted check from #2674: a Tailscale source on the LAN socket.
        assert!(misrouted_tailscale(viewer_tailscale, lan));
        assert!(misrouted_tailscale(
            "[fd7a:115c:a1e0::6f4c:e629]:9".parse().unwrap(),
            "[2402:800::1]:5".parse().unwrap()
        ));
        // Real Tailscale traffic on the Tailscale socket, and LAN traffic, are kept.
        assert!(!misrouted_tailscale(viewer_tailscale, tailnet));
        assert!(!misrouted_tailscale(viewer_lan, lan));
        // 100.128.0.0 is outside 100.64.0.0/10.
        assert!(!misrouted_tailscale("100.128.0.1:9".parse().unwrap(), lan));
    }

    #[test]
    fn only_the_same_numbered_picture_at_the_same_size_is_a_repeat() {
        let frame = |seq, width| Frame {
            data: vec![0u8; width as usize * 4 * 2].into(),
            width,
            height: 2,
            seq,
        };
        let encoded = Some((7, 4, 2));
        assert!(is_repeat(encoded, &frame(7, 4)));
        // A restarted source's picture has a new number (next_frame_seq), never an old one.
        assert!(!is_repeat(encoded, &frame(8, 4)));
        // The same number at another size is never skipped: the encoder must be rebuilt.
        assert!(!is_repeat(encoded, &frame(7, 6)));
        // Unnumbered frames and the first frame always count as new.
        assert!(!is_repeat(encoded, &frame(0, 4)));
        assert!(!is_repeat(None, &frame(7, 4)));
        let (a, b) = (
            crate::platform::next_frame_seq(),
            crate::platform::next_frame_seq(),
        );
        assert!(b > a && a > 0);
    }

    #[test]
    fn quality_messages_are_recognised() {
        let level = |text: &str| Quality::from_message(text);
        assert_eq!(
            level(r#"{"type":"quality","level":"high"}"#),
            Some(Quality::High)
        );
        assert_eq!(
            level(r#"{"type":"quality","level":"balanced"}"#),
            Some(Quality::Balanced)
        );
        assert_eq!(
            level(r#"{"type":"quality","level":"saver"}"#),
            Some(Quality::Saver)
        );
        // Anything else goes on to the server as before.
        assert_eq!(level(r#"{"type":"quality","level":"ultra"}"#), None);
        assert_eq!(level(r#"{"id":3,"type":"actions","actions":[]}"#), None);
        assert_eq!(level("not json"), None);
        assert!(Quality::High.max_width() > Quality::Balanced.max_width());
        assert!(Quality::Balanced.bitrate() > Quality::Saver.bitrate());
    }

    #[test]
    fn a_lost_display_is_said_once_and_the_primary_is_never_lost() {
        let gone = || anyhow::Error::from(crate::displays::DisplayNotFound("2".into()));
        let shown = Some("2".to_string());
        let mut reported = false;
        assert_eq!(lost_display(&shown, &mut reported, &gone()), Some("2".into()));
        assert_eq!(lost_display(&shown, &mut reported, &gone()), None);
        // Another failure (a locked screen) is not a lost display.
        let mut reported = false;
        assert_eq!(lost_display(&shown, &mut reported, &anyhow!("locked")), None);
        assert!(!reported);
        // The primary (no display named) is never reported.
        assert_eq!(lost_display(&None, &mut false, &gone()), None);
    }

    #[test]
    fn frame_rate_is_held_to_the_cap() {
        let rate = |fps| {
            Options {
                fps,
                ..Options::default()
            }
            .frame_rate()
        };
        assert_eq!(Options::default().frame_rate(), MAX_FPS);
        assert_eq!(rate(10), 10);
        assert_eq!(rate(30), MAX_FPS);
        assert_eq!(rate(60), MAX_FPS);
        assert_eq!(rate(0), 1);
    }

    #[test]
    fn tailscale_addresses_are_not_asked_about() {
        assert!(is_private_lan("192.168.11.67".parse().unwrap()));
        assert!(!is_private_lan("100.125.230.40".parse().unwrap()));
    }

    fn sha1_hmac() -> &'static dyn Sha1HmacProvider {
        str0m::crypto::from_feature_flags().sha1_hmac_provider
    }

    fn signed(message: StunMessage, password: &str) -> Vec<u8> {
        let hmac = sha1_hmac();
        let mut buf = vec![0u8; 1500];
        let n = message
            .to_bytes(Some(password.as_bytes()), &mut buf, |key, payloads| {
                hmac.sha1_hmac(key, payloads)
            })
            .unwrap();
        buf.truncate(n);
        buf
    }

    /// A viewer's check (controlling, as browsers are), USE-CANDIDATE when `nominate`.
    fn viewer_check(username: &str, password: &str, nominate: bool) -> Vec<u8> {
        let check = StunMessage::binding_request(username, TransId::new(), true, 7, 1234, nominate);
        signed(check, password)
    }

    fn answer(trans_id: TransId, mapped: SocketAddr, password: &str) -> Vec<u8> {
        signed(StunMessage::binding_reply(trans_id, mapped), password)
    }

    fn is_nomination(packet: &[u8]) -> bool {
        StunMessage::parse(packet).unwrap().use_candidate()
    }

    /// The host's side of a live view, answering a str0m viewer's offer, with its gate.
    struct Host {
        rtc: Rtc,
        gate: NominationGate,
        address: SocketAddr,
        /// "host-ufrag:viewer-ufrag", the USERNAME of the viewer's checks.
        username: String,
        local_password: String,
        remote_password: String,
        now: Instant,
    }

    impl Host {
        fn new() -> Self {
            let now = Instant::now();
            let viewer_address: SocketAddr = "192.168.11.67:54217".parse().unwrap();
            let mut viewer = RtcConfig::new()
                .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
                .build(now);
            viewer.add_local_candidate(Candidate::host(viewer_address, "udp").unwrap());
            let mut change = viewer.sdp_api();
            change.add_channel("control".into());
            let (offer, _pending) = change.apply().unwrap();
            let offer = offer.to_sdp_string();

            let address: SocketAddr = "192.168.11.73:59000".parse().unwrap();
            let mut rtc = RtcConfig::new()
                .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
                .build(now);
            rtc.add_local_candidate(Candidate::host(address, "udp").unwrap());
            rtc.sdp_api()
                .accept_offer(SdpOffer::from_sdp_string(&offer).unwrap())
                .unwrap();
            let local = rtc.direct_api().local_ice_credentials();
            let viewer_ufrag = offer
                .lines()
                .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
                .unwrap();
            let remote_password = ice_password(&offer).unwrap();
            Self {
                gate: NominationGate::new(local.pass.clone(), remote_password.clone(), sha1_hmac()),
                rtc,
                address,
                username: format!("{}:{viewer_ufrag}", local.ufrag),
                local_password: local.pass,
                remote_password,
                now,
            }
        }

        /// One packet from `source` through the gate into str0m, as `Session::run` does.
        /// Whether the gate passed it, as it came or as str0m got it.
        fn receive(&mut self, source: SocketAddr, packet: &[u8]) -> Option<Vec<u8>> {
            let data = self
                .gate
                .received(source, self.address, packet)?
                .into_owned();
            let receive = Receive {
                proto: Protocol::Udp,
                source,
                destination: self.address,
                contents: data.as_slice().try_into().unwrap(),
            };
            self.rtc
                .handle_input(Input::Receive(self.now, receive))
                .unwrap();
            Some(data)
        }

        /// What str0m sends in the next second, each seen by the gate as `Session::run` does.
        fn transmits(&mut self) -> Vec<(SocketAddr, Vec<u8>)> {
            let mut sent = Vec::new();
            for _ in 0..20 {
                loop {
                    match self.rtc.poll_output().unwrap() {
                        Output::Transmit(t) => {
                            self.gate.sent(t.source, t.destination, &t.contents);
                            sent.push((t.destination, t.contents.to_vec()));
                        }
                        Output::Event(_) => {}
                        Output::Timeout(_) => break,
                    }
                }
                self.now += Duration::from_millis(50);
                self.rtc.handle_input(Input::Timeout(self.now)).unwrap();
            }
            sent
        }
    }

    /// The check str0m sent to `to`, if any.
    fn check_to(sent: &[(SocketAddr, Vec<u8>)], to: SocketAddr) -> Option<TransId> {
        sent.iter()
            .filter(|(destination, _)| *destination == to)
            .filter_map(|(_, packet)| StunMessage::parse(packet).ok())
            .find(|message| message.is_binding_request())
            .map(|message| message.trans_id())
    }

    #[test]
    fn a_nomination_from_a_new_address_is_checked_back_then_taken() {
        let mut host = Host::new();
        // Not in the offer: peer-reflexive, known only from this nomination.
        let tether: SocketAddr = "192.168.1.177:36970".parse().unwrap();
        let nomination = viewer_check(&host.username, &host.local_password, true);

        // str0m gets it as an ordinary check, still signed with our password...
        let passed = host.receive(tether, &nomination).unwrap();
        assert!(!is_nomination(&passed));
        let message = StunMessage::parse(&passed).unwrap();
        assert!(
            message.verify(host.local_password.as_bytes(), |k, p| sha1_hmac()
                .sha1_hmac(k, p))
        );
        assert_eq!(
            message.trans_id(),
            StunMessage::parse(&nomination).unwrap().trans_id()
        );
        // ...so it learns the address and checks it back.
        let check = check_to(&host.transmits(), tether).expect("no check back to the new address");

        // The viewer answers: the path works both ways, and the repeated nomination is taken.
        host.receive(tether, &answer(check, host.address, &host.remote_password));
        let nomination = viewer_check(&host.username, &host.local_password, true);
        assert_eq!(host.receive(tether, &nomination).unwrap(), nomination);
    }

    #[test]
    fn a_nomination_from_an_address_that_never_answers_is_never_taken() {
        let mut host = Host::new();
        let tether: SocketAddr = "192.168.1.177:36970".parse().unwrap();
        for _ in 0..3 {
            let nomination = viewer_check(&host.username, &host.local_password, true);
            let passed = host.receive(tether, &nomination).unwrap();
            assert!(!is_nomination(&passed));
            host.transmits();
        }
    }

    #[test]
    fn only_a_signed_answer_to_our_own_check_proves_a_path() {
        let mut host = Host::new();
        let tether: SocketAddr = "192.168.1.177:36970".parse().unwrap();
        let elsewhere: SocketAddr = "192.168.11.67:54217".parse().unwrap();
        host.receive(
            tether,
            &viewer_check(&host.username, &host.local_password, false),
        );
        let check = check_to(&host.transmits(), tether).unwrap();
        let unsigned = {
            let mut buf = vec![0u8; 1500];
            let n = StunMessage::binding_reply(check, host.address)
                .to_bytes(None, &mut buf, |k, p| sha1_hmac().sha1_hmac(k, p))
                .unwrap();
            buf.truncate(n);
            buf
        };
        let not_proof = [
            // A transaction we never started.
            (
                tether,
                answer(TransId::new(), host.address, &host.remote_password),
            ),
            // Our transaction, answered from another address.
            (
                elsewhere,
                answer(check, host.address, &host.remote_password),
            ),
            // Signed with the wrong password, or not at all.
            (
                tether,
                answer(check, host.address, "not-the-viewers-password"),
            ),
            (tether, unsigned),
            // A bare success header: type, length, cookie and transaction only.
            (tether, {
                let mut header = vec![0x01, 0x01, 0, 0, 0x21, 0x12, 0xa4, 0x42];
                header.extend_from_slice(&[7; 12]);
                header
            }),
        ];
        for (source, packet) in not_proof {
            host.gate.received(source, host.address, &packet);
            let nomination = viewer_check(&host.username, &host.local_password, true);
            let passed = host
                .gate
                .received(tether, host.address, &nomination)
                .unwrap();
            assert!(is_nomination(&nomination) && !is_nomination(&passed));
        }
        assert!(host.gate.proven.is_empty());
    }

    #[test]
    fn checks_awaiting_an_answer_are_bounded() {
        let mut gate = NominationGate::new("local".into(), "remote".into(), sha1_hmac());
        let from: SocketAddr = "192.168.11.73:59000".parse().unwrap();
        let to: SocketAddr = "192.168.11.67:54217".parse().unwrap();
        for _ in 0..OUTSTANDING_CHECKS + 10 {
            gate.sent(from, to, &viewer_check("a:b", "remote", false));
        }
        assert_eq!(gate.outstanding.len(), OUTSTANDING_CHECKS);
    }
}

#[cfg(test)]
mod frame_cost {
    use super::*;

    /// Time per step for one changing 1280x720 frame: `cargo test --release frame_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn per_frame_cost() {
        let (w, h) = (1280usize, 720usize);
        let src = rtx_screens::image::Bgra {
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            data: &vec![0u8; 1920 * 1080 * 4],
        };
        let n = 60;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(crate::platform::Frame::scaled(&src, w as u32));
        }
        println!(
            "scale 1920->1280: {:.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        let mut frames: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                (0..w * h * 4)
                    .map(|p| ((p * 7 + i * 13) % 251) as u8)
                    .collect()
            })
            .collect();
        let t = Instant::now();
        let mut yuv = Vec::new();
        for f in &frames {
            yuv.push(bgra_to_i420(f, w, h));
        }
        println!(
            "bgra_to_i420: {:.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        let mut config = EncoderConfig::new(
            w,
            h,
            ImageFormat::I420,
            CodecConfig::Vp8(Vp8Config {
                static_threshold: Some(1000),
                noise_sensitivity: Some(0),
                ..Vp8Config::default()
            }),
        );
        config.fps_numerator = 30;
        config.target_bitrate = 2_500_000;
        config.deadline = EncodingDeadline::Realtime;
        config.rate_control = RateControlMode::Vbr;
        config.cpu_used = Some(8);
        config.error_resilient = true;
        config.max_quantizer = 56;
        let mut enc = Encoder::new(config).unwrap();
        let t = Instant::now();
        for (y, u, v) in &yuv {
            enc.encode(
                &ImageData::I420 { y, u, v },
                &EncodeOptions {
                    force_keyframe: false,
                },
            )
            .unwrap();
            while enc.next_frame().is_some() {}
        }
        println!(
            "vp8 encode (changing noise): {:.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        frames.clear();
    }
}

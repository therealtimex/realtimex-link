//! `rtx-control`: RealTimeX Remote Control's desktop side. One JSON request per stdin line, one
//! JSON response per stdout line (`server/utils/remoteControl/backend.js` is the client):
//!
//! ```text
//! {"id":1,"op":"start"}                       -> {"id":1,"ok":true,"size":[w,h]}
//! {"id":2,"op":"screenshot","live":false}    -> {"id":2,"ok":true,"mime":"image/jpeg","width":..,"height":..,"data":"<base64>","thumb":"<hex>"}
//! {"id":3,"op":"move","x":10,"y":20}         (screenshot pixels; "spaceWidth"/"spaceHeight"
//!                                             give another picture size, e.g. a live view's)
//! {"id":4,"op":"button","button":"left","pressed":true}
//! {"id":5,"op":"keysym","keysym":65293,"pressed":true}   (X11 keysyms)
//! {"id":6,"op":"scroll","dx":0,"dy":3}       (wheel steps, positive = down/right)
//! {"id":7,"op":"paste_text","text":"…","terminal":false}
//! {"id":14,"op":"paste_image","png":"<base64>","dib":"<base64>"}   (onto the clipboard as PNG,
//!                                             on Windows with "dib" as CF_DIB too; then the paste key)
//! {"id":15,"op":"paste_files","paths":["/abs/a.pdf"]} (onto the clipboard as files, then the paste key)
//! {"id":16,"op":"clipboard_seq"}             -> {"seq":12,"own":false}  (changes with every copy; own: this helper's write)
//! {"id":17,"op":"clipboard_read"}            -> {"kind":"text","text":…} | {"kind":"image","mime":…,"size":…,"data":"<base64>"}
//!                                               | {"kind":"files","files":[{"path","name","size","dir"}]} | {"kind":"empty"}
//! {"id":8,"op":"stop"}
//! {"id":9,"op":"permissions"}               -> {"screenRecording":true,"accessibility":false} (null: n/a)
//! {"id":10,"op":"request_permission","name":"accessibility"}  (macOS prompt + System Settings)
//! {"id":11,"op":"peer_offer","sdp":"…","maxWidth":1280,"fps":15} -> {"session":1,"sdp":"<answer>"}
//! {"id":12,"op":"peer_send","session":1,"data":"…"}   (text to the viewer's DataChannel)
//! {"id":13,"op":"peer_close","session":1}
//! ```
//!
//! A peer session (see [`crate::peer`]) is the WebRTC live view: the screen as VP8 and a
//! DataChannel, directly between the two machines. It reports on stdout without a request:
//!
//! ```text
//! {"id":null,"event":"peer_state","session":1,"state":"connected"|"closed"}
//! {"id":null,"event":"peer_message","session":1,"data":"…"}   (text from the DataChannel)
//! ```
//!
//! Errors answer `{"id":..,"ok":false,"error":"…"}`, with a `"code"` when the server can say
//! what to do about it ([`platform::InputError`], e.g. `input-blocked` on Windows). Pointer
//! coordinates are pixels of the latest screenshot, mapped onto the main display.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::peer::{self, Peer, ToMain};
use crate::platform::{self, Button, Input};

/// Screenshots are at most this wide unless the request asks otherwise.
const DEFAULT_MAX_WIDTH: u32 = 1920;
/// How often the stall watch looks for a capture request that will never answer.
const STALL_CHECK: Duration = Duration::from_secs(5);

/// A screenshot or window list request this process gave up on never answered (a lost
/// ScreenCaptureKit callback), so every later screenshot and the live view are refused until
/// the process ends. `run` ends with it, and `main` exits with its own code so rtx-linkd knows
/// why; rtx-linkd starts a new rtx-control with the next request. Without this, the 5-minute
/// idle stop never comes while a viewer keeps retrying, and Remote Control stays blank.
#[derive(Debug)]
pub struct CaptureStalled {
    op: &'static str,
    held: Duration,
}

impl std::fmt::Display for CaptureStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "screen capture stalled: a {} request has not answered for {:.0} s; exiting so a new rtx-control can capture again",
            self.op,
            self.held.as_secs_f64()
        )
    }
}

impl std::error::Error for CaptureStalled {}

struct Controller {
    input: Option<Input>,
    /// Size of the latest screenshot, the space request coordinates are in.
    shot_size: Option<(u32, u32)>,
    /// Keys and buttons pressed and not yet released. They are released when the session ends
    /// (stop, an error that drops it, or stdin closing), so a dropped controller can't leave
    /// Ctrl or a mouse button held down on the desktop.
    held_keys: BTreeSet<u32>,
    held_buttons: Vec<Button>,
    /// Open WebRTC live views by session number.
    peers: HashMap<u64, Peer>,
    next_peer: u64,
    to_main: Sender<Msg>,
    /// AppKit's window-server connection, made on the first request that needs the screen or
    /// input. A permissions check never makes it: on macOS that connection puts the helper in
    /// the Dock for a moment (it lives inside an app bundle), and checks run often.
    window_server: bool,
}

/// What the control loop waits for: a request line, stdin closing, a peer session, or the
/// stall watch.
enum Msg {
    Line(String),
    Eof,
    Peer(ToMain),
    Stalled(CaptureStalled),
}

impl Controller {
    fn input(&mut self) -> Result<&mut Input> {
        if self.input.is_none() {
            self.input = Some(Input::new()?);
        }
        Ok(self.input.as_mut().unwrap())
    }

    /// Release everything still held, best effort, then forget it.
    fn release_held(&mut self) {
        let keys: Vec<u32> = std::mem::take(&mut self.held_keys).into_iter().collect();
        let buttons = std::mem::take(&mut self.held_buttons);
        if let Some(input) = self.input.as_mut() {
            for keysym in keys.into_iter().rev() {
                let _ = input.keysym(keysym, false);
            }
            for button in buttons {
                let _ = input.button(button, false);
            }
        }
    }

    /// End the input session, releasing anything still held first.
    fn end_session(&mut self) {
        self.release_held();
        self.input = None;
    }

    /// The main display. On GNOME Wayland the frame comes from the remote-desktop session
    /// itself; elsewhere it needs no input permission (a view-only client works with Screen
    /// Recording alone on macOS).
    fn screenshot(&mut self, max_width: u32, live: bool) -> Result<platform::Shot> {
        let shot = if platform::screenshot_needs_input() {
            self.input()?.screenshot(max_width, live)?
        } else {
            platform::capture_display(max_width)?
        };
        // A live view's smaller frames say their own size with each click (`space`).
        if !live {
            self.shot_size = Some((shot.width, shot.height));
        }
        Ok(shot)
    }

    /// Screenshot pixels to the input system's coordinates (points on macOS, frame pixels on
    /// GNOME Wayland). `space` is the size of the picture the point is in, when not the
    /// latest screenshot.
    fn screen_point(&mut self, x: f64, y: f64, space: Option<(u32, u32)>) -> Result<(f64, f64)> {
        if space.is_none() && self.shot_size.is_none() {
            // No screenshot yet: take one so coordinates have a meaning.
            self.screenshot(DEFAULT_MAX_WIDTH, false)?;
        }
        let (sw, sh) = space.or(self.shot_size).unwrap();
        let (w, h) = self
            .input()?
            .screen_size()
            .unwrap_or((sw as f64, sh as f64));
        Ok((x * w / sw as f64, y * h / sh as f64))
    }

    fn handle(&mut self, request: &Value) -> Result<Value> {
        let op = request["op"].as_str().unwrap_or("");
        if !self.window_server && !matches!(op, "permissions" | "status") {
            platform::init_window_server();
            self.window_server = true;
        }
        match op {
            "start" => {
                let size = self.input()?.screen_size();
                Ok(json!({ "size": size.map(|(w, h)| [w, h]), "backend": platform::PLATFORM }))
            }
            "status" => Ok(json!({ "active": self.input.is_some() })),
            // Checked without prompting; null where the platform has no such permission.
            "permissions" => {
                let (screen_recording, accessibility) = platform::control_permissions();
                Ok(json!({
                    "screenRecording": screen_recording,
                    "accessibility": accessibility,
                    "app": platform::permission_owner(),
                }))
            }
            "request_permission" => {
                let name = request["name"].as_str().unwrap_or("");
                platform::request_control_permission(name)?;
                Ok(json!({}))
            }
            "stop" => {
                self.end_session();
                Ok(json!({}))
            }
            "screenshot" => {
                let max_width = request["maxWidth"]
                    .as_u64()
                    .unwrap_or(DEFAULT_MAX_WIDTH as u64) as u32;
                // A live view's frame: no waiting for one newer than the request.
                let live = request["live"].as_bool().unwrap_or(false);
                let shot = self.screenshot(max_width, live)?;
                Ok(json!({
                    "mime": "image/jpeg",
                    "width": shot.width,
                    "height": shot.height,
                    "data": base64(&shot.jpeg),
                    // Same value for the same picture: a live view skips unchanged frames.
                    "thumb": thumb_hash(&shot.thumb),
                }))
            }
            "move" => {
                let (x, y) = coordinates(request)?;
                let space = match (
                    request["spaceWidth"].as_u64(),
                    request["spaceHeight"].as_u64(),
                ) {
                    (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as u32, h as u32)),
                    _ => None,
                };
                let (x, y) = self.screen_point(x, y, space)?;
                self.input()?.move_to(x, y)?;
                Ok(json!({}))
            }
            "button" => {
                let button = match request["button"].as_str().unwrap_or("left") {
                    "left" => Button::Left,
                    "right" => Button::Right,
                    "middle" => Button::Middle,
                    other => bail!("Unknown button: {other}"),
                };
                let pressed = request["pressed"].as_bool().unwrap_or(false);
                self.input()?.button(button, pressed)?;
                self.held_buttons.retain(|&held| held != button);
                if pressed {
                    self.held_buttons.push(button);
                }
                Ok(json!({}))
            }
            "keysym" => {
                let keysym = request["keysym"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("keysym is required"))?
                    as u32;
                let pressed = request["pressed"].as_bool().unwrap_or(false);
                self.input()?.keysym(keysym, pressed)?;
                if pressed {
                    self.held_keys.insert(keysym);
                } else {
                    self.held_keys.remove(&keysym);
                }
                Ok(json!({}))
            }
            "scroll" => {
                let dx = request["dx"].as_i64().unwrap_or(0) as i32;
                let dy = request["dy"].as_i64().unwrap_or(0) as i32;
                self.input()?.scroll(dx, dy)?;
                Ok(json!({}))
            }
            "paste_text" => {
                let text = request["text"].as_str().unwrap_or("");
                if text.is_empty() {
                    bail!("text is required");
                }
                let terminal = request["terminal"].as_bool().unwrap_or(false);
                self.input()?.paste_text(text, terminal)?;
                Ok(json!({}))
            }
            "paste_image" => {
                let png = unbase64(request["png"].as_str().unwrap_or(""))
                    .ok_or_else(|| anyhow!("png must be base64"))?;
                if !png.starts_with(PNG_SIGNATURE) {
                    bail!("png is not a PNG image");
                }
                if png.len() > MAX_PASTE_IMAGE_BYTES {
                    bail!(
                        "the image is too large to paste ({} bytes, at most {MAX_PASTE_IMAGE_BYTES})",
                        png.len()
                    );
                }
                // The same picture as a Windows bitmap (CF_DIB), made by the host (#2747).
                let dib = match request["dib"].as_str() {
                    Some(dib) => Some(unbase64(dib).ok_or_else(|| anyhow!("dib must be base64"))?),
                    None => None,
                };
                if let Some(dib) = &dib {
                    if dib.len() < 40
                        || dib.len() > MAX_PASTE_DIB_BYTES
                        || dib[0..4] != [40, 0, 0, 0]
                    {
                        bail!("dib is not a BITMAPINFOHEADER bitmap of at most {MAX_PASTE_DIB_BYTES} bytes");
                    }
                }
                self.input()?.paste_image(&png, dib.as_deref())?;
                Ok(json!({}))
            }
            "paste_files" => {
                let paths: Vec<String> = request["paths"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|p| p.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if paths.is_empty() || paths.len() > MAX_PASTE_FILES {
                    bail!("paths must list 1 to {MAX_PASTE_FILES} files");
                }
                if let Some(missing) = paths.iter().find(|p| {
                    !std::path::Path::new(p).is_absolute() || !std::path::Path::new(p).exists()
                }) {
                    bail!("not an existing file: {missing}");
                }
                self.input()?.paste_files(&paths)?;
                Ok(json!({}))
            }
            "clipboard_seq" => {
                let (seq, own) = self.input()?.clipboard_seq()?;
                Ok(json!({ "seq": seq, "own": own }))
            }
            "clipboard_read" => Ok(clipboard_json(self.input()?.clipboard_read()?)),
            "peer_offer" => {
                let sdp = request["sdp"]
                    .as_str()
                    .ok_or_else(|| anyhow!("sdp is required"))?;
                let defaults = peer::Options::default();
                let options = peer::Options {
                    max_width: request["maxWidth"]
                        .as_u64()
                        .map_or(defaults.max_width, |w| w.clamp(320, 1920) as u32),
                    fps: request["fps"]
                        .as_u64()
                        .map_or(defaults.fps, |f| f.clamp(1, peer::MAX_FPS as u64) as u32),
                    ..defaults
                };
                self.next_peer += 1;
                let session = self.next_peer;
                let (to_main_tx, to_main_rx) = mpsc::channel();
                let forward = self.to_main.clone();
                std::thread::spawn(move || {
                    for message in to_main_rx {
                        if forward.send(Msg::Peer(message)).is_err() {
                            return;
                        }
                    }
                });
                let (handle, answer) = peer::start(session, sdp, options, to_main_tx)?;
                self.peers.insert(session, handle);
                Ok(json!({ "session": session, "sdp": answer }))
            }
            "peer_send" => {
                let session = request["session"].as_u64().unwrap_or(0);
                let data = request["data"].as_str().unwrap_or("");
                match self.peers.get(&session) {
                    Some(handle) => handle.send(data.to_string()),
                    None => bail!("no live view session {session}"),
                }
                Ok(json!({}))
            }
            "peer_close" => {
                let session = request["session"].as_u64().unwrap_or(0);
                self.peers.remove(&session);
                Ok(json!({}))
            }
            other => bail!("Unknown op: {other}"),
        }
    }
}

fn coordinates(request: &Value) -> Result<(f64, f64)> {
    match (request["x"].as_f64(), request["y"].as_f64()) {
        (Some(x), Some(y)) if x >= 0.0 && y >= 0.0 => Ok((x, y)),
        _ => bail!("x and y must be non-negative numbers"),
    }
}

/// The first bytes of every PNG file.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Files pasted at once: a drop or a paste of copied files.
const MAX_PASTE_FILES: usize = 100;
/// A pasted image's limit, agreed with the Human (#2682): screenshots are a few MB, and it keeps
/// X11 within one property write (about 16 MB with BIG-REQUESTS).
const MAX_PASTE_IMAGE_BYTES: usize = 15 * 1024 * 1024;
/// A pasted image's bitmap: 32 bits a pixel, about 4096 × 4096.
const MAX_PASTE_DIB_BYTES: usize = 64 * 1024 * 1024;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// FNV-1a of the grayscale thumbnail, as hex.
fn thumb_hash(thumb: &[u8]) -> String {
    let hash = thumb.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| {
        (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A clipboard's content as JSON for the server (#2682). An image over the paste limit is
/// described, not sent.
fn clipboard_json(content: platform::ClipboardContent) -> Value {
    use platform::ClipboardContent;
    match content {
        ClipboardContent::Empty => json!({ "kind": "empty" }),
        ClipboardContent::Text(text) => json!({ "kind": "text", "text": text }),
        ClipboardContent::Image { bytes, mime } if bytes.len() > MAX_PASTE_IMAGE_BYTES => {
            json!({ "kind": "image", "mime": mime, "size": bytes.len(), "tooLarge": true })
        }
        ClipboardContent::Image { bytes, mime } => {
            json!({ "kind": "image", "mime": mime, "size": bytes.len(), "data": base64(&bytes) })
        }
        ClipboardContent::Files(paths) => {
            let files: Vec<Value> = paths
                .iter()
                .map(|path| {
                    let meta = std::fs::metadata(path).ok();
                    json!({
                        "path": path,
                        "name": std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                        "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                        "dir": meta.as_ref().is_some_and(|m| m.is_dir()),
                    })
                })
                .collect();
            json!({ "kind": "files", "files": files })
        }
    }
}

/// The standard base64 alphabet back to bytes; padding optional, whitespace not allowed.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut count) = (0u32, 0);
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&a| a == byte)? as u32;
        bits = bits << 6 | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

fn write_line(out: &mut impl Write, value: &Value) {
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Serve requests until stdin closes.
pub fn run() -> Result<()> {
    let (to_main, inbox) = mpsc::channel();
    let mut controller = Controller {
        input: None,
        shot_size: None,
        held_keys: BTreeSet::new(),
        held_buttons: Vec::new(),
        peers: HashMap::new(),
        next_peer: 0,
        to_main: to_main.clone(),
        window_server: false,
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    write_line(
        &mut out,
        &json!({ "id": null, "ok": true, "ready": true, "backend": platform::PLATFORM }),
    );
    // Only macOS captures through the one-request gate a lost callback can hold for good.
    if cfg!(target_os = "macos") {
        let watch = to_main.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(STALL_CHECK);
            if let Some((op, held)) = rtx_screens::stalled_request() {
                let _ = watch.send(Msg::Stalled(CaptureStalled { op, held }));
                return;
            }
        });
    }
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if to_main.send(Msg::Line(line)).is_err() {
                return;
            }
        }
        let _ = to_main.send(Msg::Eof);
    });
    for message in inbox {
        let line = match message {
            Msg::Line(line) => line,
            Msg::Eof => break,
            Msg::Stalled(stalled) => {
                // Release held keys and buttons and end the live views before exiting, as when
                // stdin closes.
                controller.peers.clear();
                controller.end_session();
                return Err(stalled.into());
            }
            Msg::Peer(event) => {
                controller.on_peer(event, &mut out);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_line(
                    &mut out,
                    &json!({ "id": null, "ok": false, "error": "Invalid JSON" }),
                );
                continue;
            }
        };
        let id = request["id"].clone();
        let op = request["op"].as_str().unwrap_or("");
        let result = controller.handle(&request);
        if result.is_err() && !matches!(op, "start" | "peer_offer" | "peer_send" | "peer_close") {
            // The desktop may have ended the session (GNOME: the user stopped sharing); start
            // a fresh one with the next request instead of failing forever.
            controller.end_session();
        }
        write_line(&mut out, &response(id, result));
    }
    controller.peers.clear();
    controller.end_session();
    Ok(())
}

fn response(id: Value, result: Result<Value>) -> Value {
    match result {
        Ok(Value::Object(mut fields)) => {
            fields.insert("id".into(), id);
            fields.insert("ok".into(), Value::Bool(true));
            Value::Object(fields)
        }
        Ok(_) => json!({ "id": id, "ok": true }),
        Err(error) => match error.downcast_ref::<platform::InputError>() {
            Some(input) => json!({
                "id": id,
                "ok": false,
                "error": format!("{error:#}"),
                "code": input.code,
            }),
            None => json!({ "id": id, "ok": false, "error": format!("{error:#}") }),
        },
    }
}

impl Controller {
    /// A peer session's request (a frame) or report (DataChannel text, state).
    fn on_peer(&mut self, event: ToMain, out: &mut impl Write) {
        match event {
            ToMain::Frame { max_width, reply } => {
                let frame = if platform::screenshot_needs_input() {
                    self.input().and_then(|input| input.frame(max_width))
                } else {
                    platform::capture_display_frame(max_width)
                };
                let _ = reply.send(peer::SessionIn::Frame(frame));
            }
            ToMain::Message { session, text } => write_line(
                out,
                &json!({ "id": null, "event": "peer_message", "session": session, "data": text }),
            ),
            ToMain::State { session, state } => {
                if state == "closed" {
                    self.peers.remove(&session);
                }
                write_line(
                    out,
                    &json!({ "id": null, "event": "peer_state", "session": session, "state": state }),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbase64_reverses_base64() {
        for bytes in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0, 255, 128, 7],
        ] {
            assert_eq!(unbase64(&base64(bytes)).as_deref(), Some(bytes));
        }
        assert_eq!(unbase64("Zm9v").as_deref(), Some(&b"foo"[..]));
        assert_eq!(unbase64("Zm9v!"), None);
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn coordinates_must_be_non_negative_numbers() {
        assert!(coordinates(&json!({"x": 1, "y": 2.5})).is_ok());
        assert!(coordinates(&json!({"x": -1, "y": 2})).is_err());
        assert!(coordinates(&json!({"x": "1", "y": 2})).is_err());
    }
}

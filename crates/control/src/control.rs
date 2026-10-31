//! `rtx-control`: RealTimeX Remote Control's desktop side. One JSON request per stdin line, one
//! JSON response per stdout line (`server/utils/remoteControl/backend.js` is the client):
//!
//! ```text
//! {"id":1,"op":"start"}                       -> {"id":1,"ok":true,"size":[w,h]}
//!     ("virtualDisplays":n: a headless GNOME Wayland session records n virtual monitors, 1–4)
//! {"id":2,"op":"screenshot","live":false}    -> {"id":2,"ok":true,"mime":"image/jpeg","width":..,"height":..,"data":"<base64>","thumb":"<hex>"}
//! {"id":3,"op":"move","x":10,"y":20}         (screenshot pixels; "spaceWidth"/"spaceHeight"
//!                                             give another picture size, e.g. a live view's)
//! {"id":4,"op":"button","button":"left","pressed":true}
//! {"id":5,"op":"keysym","keysym":65293,"pressed":true}   (X11 keysyms)
//! {"id":18,"op":"key","code":"KeyA","pressed":true}   (a physical key by its KeyboardEvent.code,
//!     realtimex-link#7: a down for a held key is its repeat; an unknown code or an up for a key
//!     not held does nothing)
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
//! {"id":19,"op":"displays"}                  -> {"displays":[{"id","name","primary","x","y","width","height","scale"}…]}
//! {"id":20,"op":"peer_display","session":1,"display":"<id>"} -> {"display":"<id>","width":..,"height":..}
//! ```
//!
//! Which display (realtimex-link#12): `screenshot`, `move` and `peer_offer` take `"display":"<id>"`
//! (an id from `displays`); without one they mean the primary display, as before. A display that
//! isn't attached answers the code `display-not-found`, never another display. Nothing here
//! remembers a choice: rtx-linkd keeps each live view's.
//!
//! A peer session (see [`crate::peer`]) is the WebRTC live view: the screen as VP8 and a
//! DataChannel, directly between the two machines. It reports on stdout without a request:
//!
//! ```text
//! {"id":null,"event":"peer_state","session":1,"state":"connected"|"closed"}
//! {"id":null,"event":"peer_message","session":1,"data":"…"}   (text from the DataChannel)
//! {"id":null,"event":"peer_display_lost","session":1,"display":"<id>","reason":"display_removed"}
//!     (its display went away, or "display_unavailable": it can't be captured now, and it is
//!     unlisted until the state changes; the view keeps its last picture until rtx-linkd sends
//!     `peer_display`)
//! ```
//!
//! Errors answer `{"id":..,"ok":false,"error":"…"}`, with a `"code"` when the server can say
//! what to do about it ([`platform::InputError`], e.g. `input-blocked` on Windows). Pointer
//! coordinates are pixels of the latest screenshot of that display, mapped onto it.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::codes::{physical_key, PhysicalKey};
use crate::displays::{self, Display};
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
    /// Size of the latest screenshot of each display (None: the primary), the space request
    /// coordinates on it are in.
    shot_sizes: HashMap<Option<String>, (u32, u32)>,
    /// The displays and where the pointer reaches each (realtimex-link#12).
    displays: displays::Table,
    /// Displays that can't be captured in the current state, left out of the list.
    unavailable: displays::Unavailable,
    /// Keys and buttons pressed and not yet released. They are released when the session ends
    /// (stop, an error that drops it, or stdin closing), so a dropped controller can't leave
    /// Ctrl or a mouse button held down on the desktop.
    held_keys: BTreeSet<u32>,
    held_buttons: Vec<Button>,
    /// Physical keys held, in the order they went down (#2909).
    held_codes: Vec<&'static PhysicalKey>,
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
        let codes = std::mem::take(&mut self.held_codes);
        let keys: Vec<u32> = std::mem::take(&mut self.held_keys).into_iter().collect();
        let buttons = std::mem::take(&mut self.held_buttons);
        if let Some(input) = self.input.as_mut() {
            for key in codes.into_iter().rev() {
                let _ = input.key(key, false, false);
            }
            for keysym in keys.into_iter().rev() {
                let _ = input.keysym(keysym, false);
            }
            for button in buttons {
                let _ = input.button(button, false);
            }
        }
    }

    /// When the input session has work of its own due: on Windows and macOS, the clipboard text
    /// a paste took off comes back a moment after the last paste.
    #[cfg(any(windows, target_os = "macos"))]
    fn due(&self) -> Option<std::time::Instant> {
        self.input.as_ref().and_then(Input::restore_due)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn due(&self) -> Option<std::time::Instant> {
        None
    }

    fn run_due(&mut self) {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(input) = self.input.as_mut() {
            input.restore_clipboard();
        }
    }

    /// A physical key (#2909). A down for a key already held is a repeat; an up for a key not
    /// held, or a code with no key here, does nothing.
    fn key(&mut self, code: &str, pressed: bool) -> Result<()> {
        let Some(key) = physical_key(code) else {
            return Ok(());
        };
        if pressed
            && cfg!(windows)
            && secure_attention_chord(code, self.held_codes.iter().map(|held| held.code))
        {
            secure_attention();
            return Ok(());
        }
        if !pressed && !self.held_codes.iter().any(|held| held.code == key.code) {
            return Ok(());
        }
        self.input()?;
        let input = self.input.as_mut().expect("input was just made");
        press_key(&mut self.held_codes, input, key, pressed)
    }

    /// A paste with the held physical modifiers let go first and pressed again after, so its
    /// own combo is exactly Ctrl+V or ⌘V (Shift held would make it Ctrl+Shift+V) and a held key
    /// is still held afterwards (#2909: ControlLeft held by a Mac viewer on a Windows host).
    fn paste(&mut self, paste: impl FnOnce(&mut Input) -> Result<()>) -> Result<()> {
        let modifiers: Vec<&'static PhysicalKey> = self
            .held_codes
            .iter()
            .copied()
            .filter(|key| key.modifier.is_some())
            .collect();
        let input = self.input()?;
        for key in modifiers.iter().rev() {
            input.key(key, false, false)?;
        }
        let pasted = paste(&mut *input);
        let mut pressed_again = Ok(());
        for key in &modifiers {
            if let Err(error) = input.key(key, true, false) {
                pressed_again = pressed_again.and(Err(error));
            }
        }
        pasted.and(pressed_again)
    }

    /// End the input session, releasing anything still held first.
    fn end_session(&mut self) {
        self.release_held();
        self.input = None;
        // GNOME Wayland's displays are the session's streams.
        self.displays.clear();
    }

    /// The displays as they are now: GNOME Wayland's from its remote-desktop session, others'
    /// from the system's list (no input permission needed).
    fn read_displays(&mut self) -> Result<Vec<Display>> {
        let mut list = if platform::screenshot_needs_input() {
            self.input()?.displays()?
        } else {
            platform::displays()?
        };
        // The picker's order (realtimex-ai-app#2970), whoever listed them.
        list.sort_by_key(|display| (display.info.x, display.info.y));
        // Only what can be captured now (design risk 2).
        Ok(self.unavailable.filter(list, platform::capture_state()))
    }

    /// Note a capture that failed because its display can't be captured now: it leaves the
    /// list until the state changes, and the table is read again.
    fn note_capture<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            if let Some(id) = displays::unavailable(error) {
                self.unavailable.note(id, platform::capture_state());
                self.displays.clear();
            }
        }
        result
    }

    /// The display `id` names (None: the primary), from the table while it is fresh, else
    /// read again. GNOME Wayland's come from its session each time: which stream is the primary
    /// is the session's to say, for capture and the pointer alike (review of !15, finding 1).
    fn display(&mut self, id: Option<&str>) -> Result<Display> {
        if platform::screenshot_needs_input() {
            let list = self.input()?.current()?;
            return displays::find(&list, id).cloned();
        }
        if let Some(found) = self.displays.cached(id) {
            return Ok(found.clone());
        }
        let list = self.read_displays()?;
        self.displays.set(list);
        displays::find(self.displays.list(), id).cloned()
    }

    /// `display` (None: the primary). On GNOME Wayland the frame comes from the remote-desktop
    /// session itself; elsewhere it needs no input permission (a view-only client works with
    /// Screen Recording alone on macOS).
    fn screenshot(
        &mut self,
        max_width: u32,
        live: bool,
        display: Option<&str>,
    ) -> Result<platform::Shot> {
        let shot = injected_unavailable(display).and_then(|()| {
            if platform::screenshot_needs_input() {
                self.input()?.screenshot(max_width, live, display)
            } else {
                capture(max_width, display)
            }
        });
        let shot = self.note_capture(shot)?;
        // A live view's smaller frames say their own size with each click (`space`).
        if !live {
            self.shot_sizes
                .insert(display.map(str::to_string), (shot.width, shot.height));
        }
        Ok(shot)
    }

    /// Screenshot pixels to the input system's coordinates on `display` (None: the primary):
    /// global points on macOS, virtual-desktop pixels on Windows and X11, that display's stream
    /// pixels on GNOME Wayland. `space` is the size of the picture the point is in, when not the
    /// latest screenshot of that display.
    fn screen_point(
        &mut self,
        x: f64,
        y: f64,
        space: Option<(u32, u32)>,
        display: Option<&str>,
    ) -> Result<(Display, f64, f64)> {
        let target = self.display(display)?;
        let key = display.map(str::to_string);
        if space.is_none() && !self.shot_sizes.contains_key(&key) {
            // No screenshot yet: take one so coordinates have a meaning.
            self.screenshot(DEFAULT_MAX_WIDTH, false, display)?;
        }
        let space = space
            .or_else(|| self.shot_sizes.get(&key).copied())
            .expect("a screenshot was just taken");
        let (x, y) = displays::map_point(target.rect, x, y, space);
        Ok((target, x, y))
    }

    fn handle(&mut self, request: &Value) -> Result<Value> {
        let op = request["op"].as_str().unwrap_or("");
        if !self.window_server && !matches!(op, "permissions" | "status") {
            platform::init_window_server();
            self.window_server = true;
        }
        // macOS login window (realtimex-link#11 I4, C4): the daemon alone marks a request for
        // the job; every input request says where it goes, so none falls back by accident.
        #[cfg(target_os = "macos")]
        if matches!(op, "move" | "button" | "keysym" | "key" | "scroll") {
            let via_job = request["via"].as_str() == Some("job");
            self.input()?.route(via_job);
        }
        match op {
            "start" => {
                // GNOME Wayland, headless: how many virtual monitors a new session records
                // (Link's `virtualDisplays`, realtimex-link#12; 1 when not said).
                #[cfg(target_os = "linux")]
                platform::set_virtual_displays(
                    request["virtualDisplays"].as_u64().unwrap_or(1).clamp(1, 4) as u32,
                );
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
            "displays" => {
                // Always read now: rtx-linkd asks while a live view is open to see changes.
                let list = self.read_displays()?;
                self.displays.set(list);
                let infos: Vec<&displays::DisplayInfo> =
                    self.displays.list().iter().map(|d| &d.info).collect();
                Ok(json!({ "displays": infos }))
            }
            "screenshot" => {
                let max_width = request["maxWidth"]
                    .as_u64()
                    .unwrap_or(DEFAULT_MAX_WIDTH as u64) as u32;
                // A live view's frame: no waiting for one newer than the request.
                let live = request["live"].as_bool().unwrap_or(false);
                let display = display_of(request);
                let shot = self.screenshot(max_width, live, display)?;
                let mut answer = json!({
                    "mime": "image/jpeg",
                    "width": shot.width,
                    "height": shot.height,
                    "data": base64(&shot.jpeg),
                    // Same value for the same picture: a live view skips unchanged frames.
                    "thumb": thumb_hash(&shot.thumb),
                });
                // Which display it is: the one named, else the primary's id when known.
                let shown = match display {
                    Some(id) => Some(id.to_string()),
                    None => self.display(None).ok().map(|d| d.info.id),
                };
                if let Some(shown) = shown {
                    answer["display"] = json!(shown);
                }
                Ok(answer)
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
                let (target, x, y) = self.screen_point(x, y, space, display_of(request))?;
                self.input()?.move_to(&target, x, y)?;
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
                if pressed && cfg!(windows) && secure_attention_keysyms(keysym, &self.held_keys) {
                    secure_attention();
                    return Ok(json!({}));
                }
                self.input()?.keysym(keysym, pressed)?;
                if pressed {
                    self.held_keys.insert(keysym);
                } else {
                    self.held_keys.remove(&keysym);
                }
                Ok(json!({}))
            }
            "key" => {
                let code = request["code"]
                    .as_str()
                    .ok_or_else(|| anyhow!("code is required"))?;
                let pressed = request["pressed"].as_bool().unwrap_or(false);
                self.key(code, pressed)?;
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
                self.paste(|input| input.paste_text(text, terminal))?;
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
                self.paste(|input| input.paste_image(&png, dib.as_deref()))?;
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
                self.paste(|input| input.paste_files(&paths))?;
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
                // The view's first display: it must be there now.
                let display = match display_of(request) {
                    Some(id) => Some(self.display(Some(id))?.info.id),
                    None => None,
                };
                let options = peer::Options {
                    display,
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
            // Another display for a live view (realtimex-link#12): its next frames show it.
            "peer_display" => {
                let session = request["session"].as_u64().unwrap_or(0);
                let target = self.display(display_of(request))?;
                match self.peers.get(&session) {
                    Some(handle) => handle.show(display_of(request).map(str::to_string)),
                    None => bail!("no live view session {session}"),
                }
                Ok(json!({
                    "display": target.info.id,
                    "width": target.info.width,
                    "height": target.info.height,
                }))
            }
            other => bail!("Unknown op: {other}"),
        }
    }
}

/// Ctrl+Alt+Delete (realtimex-link#10): Delete going down while a Ctrl and an Alt are held.
/// Windows never takes it from injected keys; Link's service sends it (SendSAS) instead.
fn secure_attention_chord<'a>(code: &str, held: impl Iterator<Item = &'a str>) -> bool {
    let (mut ctrl, mut alt) = (false, false);
    for held in held {
        ctrl |= matches!(held, "ControlLeft" | "ControlRight");
        alt |= matches!(held, "AltLeft" | "AltRight");
    }
    code == "Delete" && ctrl && alt
}

/// The same chord from keysyms (a `keys` combo such as "ctrl+alt+delete").
fn secure_attention_keysyms(keysym: u32, held: &BTreeSet<u32>) -> bool {
    const DELETE: u32 = 0xffff;
    let ctrl = held.contains(&0xffe3) || held.contains(&0xffe4);
    let alt = held.contains(&0xffe9) || held.contains(&0xffea);
    keysym == DELETE && ctrl && alt
}

/// Ask Link for the secure attention sequence: an event line, as the peer's are.
fn secure_attention() {
    write_line(
        &mut std::io::stdout().lock(),
        &json!({ "id": null, "event": "secure_attention" }),
    );
}

/// Where a physical key goes: the platform's input, or a test's.
trait KeyInput {
    fn key(&mut self, key: &'static PhysicalKey, pressed: bool, repeat: bool) -> Result<()>;
}

impl KeyInput for Input {
    fn key(&mut self, key: &'static PhysicalKey, pressed: bool, repeat: bool) -> Result<()> {
        Input::key(self, key, pressed, repeat)
    }
}

/// Press or let go of a physical key, keeping `held` to what may still be down: a down for a
/// held key is its repeat, an up for a key not held does nothing. A key is held from before its
/// down (a down that fails may still have pressed it) until an up succeeds, so a failed up is
/// tried again by the next up and by `release_held`.
fn press_key(
    held: &mut Vec<&'static PhysicalKey>,
    input: &mut impl KeyInput,
    key: &'static PhysicalKey,
    pressed: bool,
) -> Result<()> {
    let index = held.iter().position(|k| k.code == key.code);
    match (pressed, index) {
        (true, Some(_)) => input.key(key, true, true),
        (true, None) => {
            held.push(key);
            input.key(key, true, false)
        }
        (false, Some(index)) => {
            input.key(key, false, false)?;
            held.remove(index);
            Ok(())
        }
        (false, None) => Ok(()),
    }
}

/// For QA (realtimex-link#12): with `RTX_CONTROL_TEST_UNAVAILABLE=<display id>`, capturing that
/// display fails as unavailable, as a locked Mac's second display may. Release builds too; only
/// a tester sets it, and it only fails this process's own captures.
fn injected_unavailable(display: Option<&str>) -> Result<()> {
    match (display, std::env::var("RTX_CONTROL_TEST_UNAVAILABLE")) {
        (Some(id), Ok(injected)) if id == injected => {
            Err(displays::DisplayUnavailable(id.to_string()).into())
        }
        _ => Ok(()),
    }
}

/// The display a request names (`"display":"<id>"`); None for the primary.
fn display_of(request: &Value) -> Option<&str> {
    request["display"].as_str().filter(|id| !id.is_empty())
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
        shot_sizes: HashMap::new(),
        displays: displays::Table::default(),
        unavailable: displays::Unavailable::default(),
        held_keys: BTreeSet::new(),
        held_buttons: Vec::new(),
        held_codes: Vec::new(),
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
    loop {
        let message = match controller.due() {
            Some(due) => {
                match inbox.recv_timeout(due.saturating_duration_since(std::time::Instant::now())) {
                    Ok(message) => message,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        controller.run_due();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match inbox.recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };
        // The sign-in, lock or UAC screen may have taken input since the last request.
        platform::follow_input_desktop();
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
        let ends_session = result.as_ref().is_err_and(|error| !displays::lost(error));
        if ends_session
            && !matches!(
                op,
                "start" | "peer_offer" | "peer_send" | "peer_close" | "peer_display" | "displays"
            )
        {
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

/// A screenshot of `display` (None: the primary): on macOS within C5's limit
/// (realtimex-link#11).
fn capture(max_width: u32, display: Option<&str>) -> Result<platform::Shot> {
    #[cfg(target_os = "macos")]
    {
        let display = display.map(str::to_string);
        platform::capture_bounded(move || platform::capture_display_on(display.as_deref(), max_width))
    }
    #[cfg(not(target_os = "macos"))]
    {
        platform::capture_display_on(display, max_width)
    }
}

/// A live-view frame: on macOS within C5's limit too.
fn capture_frame(max_width: u32, display: Option<&str>) -> Result<platform::Frame> {
    #[cfg(target_os = "macos")]
    {
        let display = display.map(str::to_string);
        platform::capture_bounded(move || {
            platform::capture_display_frame_on(display.as_deref(), max_width)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        platform::capture_display_frame_on(display, max_width)
    }
}

fn response(id: Value, result: Result<Value>) -> Value {
    match result {
        Ok(Value::Object(mut fields)) => {
            fields.insert("id".into(), id);
            fields.insert("ok".into(), Value::Bool(true));
            Value::Object(fields)
        }
        Ok(_) => json!({ "id": id, "ok": true }),
        Err(error) => match error_code(&error) {
            Some(code) => json!({
                "id": id,
                "ok": false,
                "error": format!("{error:#}"),
                "code": code,
            }),
            None => json!({ "id": id, "ok": false, "error": format!("{error:#}") }),
        },
    }
}

/// The code a failure is answered with, when the server can tell what to do about it.
fn error_code(error: &anyhow::Error) -> Option<&'static str> {
    if let Some(input) = error.downcast_ref::<platform::InputError>() {
        return Some(input.code);
    }
    if displays::not_found(error) {
        return Some("display-not-found");
    }
    displays::unavailable(error).map(|_| "display-unavailable")
}

impl Controller {
    /// A peer session's request (a frame) or report (DataChannel text, state).
    fn on_peer(&mut self, event: ToMain, out: &mut impl Write) {
        match event {
            ToMain::Frame {
                max_width,
                display,
                reply,
            } => {
                let display = display.as_deref();
                let frame = injected_unavailable(display).and_then(|()| {
                    if platform::screenshot_needs_input() {
                        self.input().and_then(|input| input.frame(max_width, display))
                    } else {
                        capture_frame(max_width, display)
                    }
                });
                let frame = self.note_capture(frame);
                let _ = reply.send(peer::SessionIn::Frame(frame));
            }
            ToMain::DisplayLost {
                session,
                display,
                reason,
            } => write_line(
                out,
                &json!({ "id": null, "event": "peer_display_lost", "session": session, "display": display, "reason": reason }),
            ),
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

    /// Records what reaches the platform; fails the calls numbered in `fail` (from 0).
    #[derive(Default)]
    struct FakeInput {
        sent: Vec<(&'static str, bool, bool)>,
        fail: Vec<usize>,
        calls: usize,
    }

    impl KeyInput for FakeInput {
        fn key(&mut self, key: &'static PhysicalKey, pressed: bool, repeat: bool) -> Result<()> {
            let call = self.calls;
            self.calls += 1;
            if self.fail.contains(&call) {
                bail!("SendInput refused it");
            }
            self.sent.push((key.code, pressed, repeat));
            Ok(())
        }
    }

    #[test]
    fn ctrl_alt_delete_is_the_secure_attention_chord() {
        assert!(secure_attention_chord("Delete", ["ControlLeft", "AltLeft"].into_iter()));
        assert!(secure_attention_chord(
            "Delete",
            ["AltRight", "ShiftLeft", "ControlRight"].into_iter()
        ));
        assert!(!secure_attention_chord("Delete", ["ControlLeft"].into_iter()));
        assert!(!secure_attention_chord("Delete", ["AltLeft"].into_iter()));
        assert!(!secure_attention_chord("Backspace", ["ControlLeft", "AltLeft"].into_iter()));
        let held: BTreeSet<u32> = [0xffe3, 0xffe9].into();
        assert!(secure_attention_keysyms(0xffff, &held));
        assert!(!secure_attention_keysyms(0xff08, &held));
        assert!(!secure_attention_keysyms(0xffff, &[0xffe3].into()));
    }

    #[test]
    fn a_held_key_repeats_and_an_up_for_a_key_not_held_does_nothing() {
        let shift = physical_key("ShiftLeft").unwrap();
        let a = physical_key("KeyA").unwrap();
        let mut held = Vec::new();
        let mut input = FakeInput::default();
        press_key(&mut held, &mut input, shift, true).unwrap();
        press_key(&mut held, &mut input, a, true).unwrap();
        // The viewer's key repeat: another down.
        press_key(&mut held, &mut input, a, true).unwrap();
        assert_eq!(held, vec![shift, a]);
        press_key(&mut held, &mut input, a, false).unwrap();
        press_key(&mut held, &mut input, a, false).unwrap();
        assert_eq!(held, vec![shift]);
        assert_eq!(
            input.sent,
            vec![
                ("ShiftLeft", true, false),
                ("KeyA", true, false),
                ("KeyA", true, true),
                ("KeyA", false, false),
            ]
        );
    }

    /// Review of !6: a key is not forgotten before its up succeeds.
    #[test]
    fn a_failed_up_keeps_the_key_held_until_an_up_succeeds() {
        let shift = physical_key("ShiftLeft").unwrap();
        let mut held = Vec::new();
        let mut input = FakeInput {
            fail: vec![1],
            ..FakeInput::default()
        };
        press_key(&mut held, &mut input, shift, true).unwrap();
        assert!(press_key(&mut held, &mut input, shift, false).is_err());
        assert_eq!(held, vec![shift]);
        // The recovery up reaches the platform, and only then is the key let go.
        press_key(&mut held, &mut input, shift, false).unwrap();
        assert!(held.is_empty());
        assert_eq!(
            input.sent,
            vec![("ShiftLeft", true, false), ("ShiftLeft", false, false)]
        );
    }

    /// A down that fails may still have pressed the key: it stays held, so it is let go later.
    #[test]
    fn a_failed_down_is_still_released() {
        let a = physical_key("KeyA").unwrap();
        let mut held = Vec::new();
        let mut input = FakeInput {
            fail: vec![0],
            ..FakeInput::default()
        };
        assert!(press_key(&mut held, &mut input, a, true).is_err());
        assert_eq!(held, vec![a]);
        press_key(&mut held, &mut input, a, false).unwrap();
        assert!(held.is_empty());
        assert_eq!(input.sent, vec![("KeyA", false, false)]);
    }

    #[test]
    fn coordinates_must_be_non_negative_numbers() {
        assert!(coordinates(&json!({"x": 1, "y": 2.5})).is_ok());
        assert!(coordinates(&json!({"x": -1, "y": 2})).is_err());
        assert!(coordinates(&json!({"x": "1", "y": 2})).is_err());
    }
}

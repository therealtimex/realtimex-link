//! The session bridge (#11 M5, I4–I6; Addendum 5): `rtx-link --launchd session` →
//! `rtx-linkd launchd-session --input-fd N`, one per GUI session: root at the login window,
//! the session's user in Aqua.
//!
//! It says hello on the supervisor's socket, which hands the connection to the owner's daemon.
//! On that connection (JSON lines):
//! - bridge → daemon: `status {onConsole, secureInput}`, now and whenever it changes;
//!   `started` with a new connection for the helper's lines (SCM_RIGHTS), or `failed`; `ended`
//!   when that helper exited;
//! - daemon → bridge: `start {session}`, `stop`, `exit` (another version: launchd starts this job
//!   again from the refreshed copy).
//!
//! It runs rtx-control as its child and carries its lines, passing only what M5 allows in its
//! kind of session. rtx-control inherits the input channel to the job (`--input-fd`, C1) for the
//! requests the daemon marks `via:job`; when a helper ends, however it ends, the bridge tells
//! the job to release everything (C3). Nothing typed is logged here (I7).

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{jobinput, os, policy};
use crate::unattended::{Request, MACOS_SOCKET};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> *const std::ffi::c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const std::ffi::c_void);
    fn CFDictionaryGetValue(dictionary: *const std::ffi::c_void, key: *const std::ffi::c_void) -> *const std::ffi::c_void;
    fn CFStringCreateWithCString(allocator: *const std::ffi::c_void, text: *const libc::c_char, encoding: u32) -> *const std::ffi::c_void;
    fn CFBooleanGetValue(boolean: *const std::ffi::c_void) -> bool;
    fn CFNumberGetValue(number: *const std::ffi::c_void, kind: isize, value: *mut std::ffi::c_void) -> bool;
}

/// This session's console state: whether it has the console, and whether loginwindow holds
/// Secure Input (I4: the job posts then).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Console {
    pub on_console: bool,
    pub secure_input: bool,
}

fn console() -> Console {
    // SAFETY: CF calls on a dictionary this function owns and releases; keys made and released.
    unsafe {
        let dictionary = CGSessionCopyCurrentDictionary();
        if dictionary.is_null() {
            return Console::default();
        }
        let key = |name: &std::ffi::CStr| CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), 0x0800_0100);
        let on_key = key(c"kCGSSessionOnConsoleKey");
        let secure_key = key(c"kCGSSessionSecureInputPID");
        let on = CFDictionaryGetValue(dictionary, on_key);
        let secure = CFDictionaryGetValue(dictionary, secure_key);
        let on_console = !on.is_null() && CFBooleanGetValue(on);
        let mut pid: i32 = 0;
        // kCFNumberSInt32Type
        let secure_input = !secure.is_null()
            && CFNumberGetValue(secure, 3, (&mut pid as *mut i32).cast())
            && process_name(pid).as_deref() == Some("loginwindow");
        CFRelease(on_key);
        CFRelease(secure_key);
        CFRelease(dictionary);
        Console { on_console, secure_input }
    }
}

fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer's size is passed.
    let length = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

pub fn run(args: &[String]) -> ! {
    let input: Option<RawFd> = args
        .iter()
        .position(|arg| arg == "--input-fd")
        .and_then(|i| args.get(i + 1))
        .and_then(|fd| fd.parse().ok());
    // SAFETY: the job's channel, handed down by rtx-link; owned here, close-on-exec (C1).
    let input = input.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    if let Some(input) = &input {
        let _ = os::set_cloexec(input.as_raw_fd(), true);
    }
    let input = Arc::new(input);
    loop {
        if let Err(error) = serve(&input) {
            eprintln!("bridge: {error}");
        }
        // No supervisor yet, or no owner's daemon: try again.
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// One connection to the owner's daemon, until it closes.
fn serve(input: &Arc<Option<OwnedFd>>) -> std::io::Result<()> {
    let stream = UnixStream::connect(MACOS_SOCKET)?;
    let mut now = console();
    let hello = Request::Bridge {
        version: env!("CARGO_PKG_VERSION").to_string(),
        on_console: now.on_console,
    };
    send(&stream, &serde_json::to_value(&hello).unwrap_or_default())?;
    send_status(&stream, now)?;
    let helper: Arc<Mutex<Option<Helper>>> = Arc::new(Mutex::new(None));
    // Console changes, every second (M5): reported, and the job releases on any change (I6).
    {
        let stream = stream.try_clone()?;
        let input = input.clone();
        let helper = helper.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let next = console();
            if next != now {
                now = next;
                release(&input);
                if send_status(&stream, now).is_err() {
                    break;
                }
            }
            // A helper that ended on its own: the job lets go of what it held (C3).
            let mut slot = helper.lock().unwrap_or_else(|p| p.into_inner());
            if slot.as_mut().is_some_and(Helper::ended) {
                *slot = None;
                release(&input);
                if send(&stream, &serde_json::json!({"op": "ended"})).is_err() {
                    break;
                }
            }
        });
    }
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        let message: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        match message.get("op").and_then(|op| op.as_str()) {
            Some("start") => {
                let session = match message.get("session").and_then(|s| s.as_str()) {
                    Some("owner") => policy::Session::Owner,
                    Some("other") => policy::Session::Other,
                    _ => policy::Session::LoginWindow,
                };
                let mut slot = helper.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(old) = slot.take() {
                    old.end();
                    release(input);
                }
                match Helper::start(session, input) {
                    Ok((started, theirs)) => {
                        os::send_fd(stream.as_raw_fd(), theirs.as_raw_fd(), b"{\"op\":\"started\"}")?;
                        *slot = Some(started);
                    }
                    Err(error) => send(&stream, &serde_json::json!({"op": "failed", "error": error.to_string()}))?,
                }
            }
            Some("stop") => {
                if let Some(old) = helper.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    old.end();
                }
                release(input);
            }
            Some("exit") => {
                release(input);
                std::process::exit(1);
            }
            _ => {}
        }
    }
    if let Some(old) = helper.lock().unwrap_or_else(|p| p.into_inner()).take() {
        old.end();
    }
    release(input);
    Ok(())
}

fn send(mut stream: &UnixStream, value: &serde_json::Value) -> std::io::Result<()> {
    let mut text = value.to_string();
    text.push('\n');
    stream.write_all(text.as_bytes())
}

fn send_status(stream: &UnixStream, console: Console) -> std::io::Result<()> {
    send(
        stream,
        &serde_json::json!({"op": "status", "onConsole": console.on_console, "secureInput": console.secure_input}),
    )
}

/// Tell the job to let go of every key and button and clear the global flags (I6, C3).
fn release(input: &Option<OwnedFd>) {
    if let Some(input) = input {
        let message = jobinput::Event::Release.encode();
        // SAFETY: a datagram on the SOCK_DGRAM socketpair this process holds.
        unsafe { libc::send(input.as_raw_fd(), message.as_ptr().cast(), message.len(), 0) };
    }
}

/// rtx-control, with the bridge carrying its lines.
struct Helper {
    child: Child,
}

impl Helper {
    /// Start rtx-control and the relay; returns the daemon's end of the new connection.
    fn start(session: policy::Session, input: &Arc<Option<OwnedFd>>) -> std::io::Result<(Helper, OwnedFd)> {
        let control = std::env::current_exe()?
            .parent()
            .map(|dir| dir.join("rtx-control"))
            .ok_or_else(|| std::io::Error::other("no folder"))?;
        let (ours, theirs) = os::socketpair(libc::SOCK_STREAM)?;
        let mut command = crate::util::command(control);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let channel: &Option<OwnedFd> = input;
        if let Some(fd) = channel.as_ref().map(AsRawFd::as_raw_fd) {
            // The one intended exec that gets the channel (C1): it is fd 3 in rtx-control.
            command.arg("--input-fd").arg("3");
            // SAFETY: dup2 and fcntl only, between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn()?;
        let mut to_helper = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let from_helper = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let daemon = UnixStream::from(ours);
        // Helper → daemon, as it is.
        let mut to_daemon = daemon.try_clone()?;
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut BufReader::new(from_helper), &mut to_daemon);
        });
        // Daemon → helper, only what this session allows (M5).
        std::thread::spawn(move || {
            let mut answers = daemon.try_clone().ok();
            for line in BufReader::new(daemon).lines().map_while(Result::ok) {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let op = request.get("op").and_then(|op| op.as_str()).unwrap_or_default();
                match policy::bridge_allows(session, op) {
                    Ok(()) => {
                        if writeln!(to_helper, "{line}").is_err() {
                            break;
                        }
                    }
                    Err(code) => {
                        let refusal = serde_json::json!({
                            "id": request.get("id"),
                            "ok": false,
                            "code": code,
                            "error": "Not available in this session",
                        });
                        if let Some(answers) = answers.as_mut() {
                            let _ = writeln!(answers, "{refusal}");
                        }
                    }
                }
            }
        });
        Ok((Helper { child }, theirs))
    }

    fn ended(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    fn end(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

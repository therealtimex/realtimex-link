//! SD's login-window input probe (realtimex-link#11, Addendum 2): which way of posting keys
//! works for a LoginWindow job, tried one step at a time from SSH so a hang costs one step.
//!
//! `--launchd probe --socket <path>` serves lines on a root-only socket:
//! - `facts self|child`: uid, `launchctl procinfo`'s session lines, `CGSessionCopyCurrentDictionary` and the
//!   AX / PostEvent / ListenEvent / ScreenCapture preflights, for this job or a child of it;
//! - `post self|child <method> <keys>`: `hid`, `session`, `app-hid` (after NSApplication with
//!   the accessory policy) or `iohid` (`IOHIDPostEvent`), typing `a` for each `a` in `<keys>`
//!   and Backspace for each `b`.
//!
//! A child step runs as `--launchd probe-child …`; every step gets 3 s, then answers `hang`.

use std::ffi::{c_char, c_void, CStr};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(3);

const KEY_A: u16 = 0x00;
const KEY_DELETE: u16 = 0x33;
const HID_EVENT_TAP: u32 = 0;
const SESSION_EVENT_TAP: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> *mut c_void;
    fn CGEventPost(tap: u32, event: *mut c_void);
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFCopyDescription(object: *const c_void) -> *const c_void;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, len: isize, encoding: u32) -> bool;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IOGPoint {
    x: i16,
    y: i16,
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    static kIOMasterPortDefault: u32;
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDPostEvent(
        connect: u32,
        kind: u32,
        location: IOGPoint,
        data: *const c_void,
        version: u32,
        flags: u32,
        options: u32,
    ) -> i32;
}

extern "C" {
    static mach_task_self_: u32;
}

pub fn serve(args: &[String]) -> ! {
    let Some(socket) = flag(args, "--socket") else {
        eprintln!("probe: --socket <path> is required");
        std::process::exit(2);
    };
    let _ = std::fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("probe: binding {socket}: {error}");
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    eprintln!("probe: pid {} uid {} on {socket}", std::process::id(), unsafe {
        libc::getuid()
    });
    for stream in listener.incoming().flatten() {
        let Ok(mut out) = stream.try_clone() else {
            continue;
        };
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
            let answer = step(&words);
            if writeln!(out, "{line}: {answer}").is_err() {
                break;
            }
        }
    }
    std::process::exit(0);
}

/// `--launchd probe-child <words>`: one step in a child of the job, its answer on stdout.
pub fn child(args: &[String]) -> ! {
    println!("{}", run(args));
    std::process::exit(0);
}

fn step(words: &[String]) -> String {
    match words.get(1).map(String::as_str) {
        Some("child") => {
            let rest = &words[2..];
            let mut words = vec![words[0].clone()];
            words.extend_from_slice(rest);
            in_child(&words)
        }
        Some("self") => {
            let mut words = words.to_vec();
            words.remove(1);
            in_thread(words)
        }
        _ => "usage: facts|post self|child …".into(),
    }
}

fn in_thread(words: Vec<String>) -> String {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(run(&words));
    });
    // A hung post stays on its thread; the probe answers and goes on.
    receive.recv_timeout(LIMIT).unwrap_or_else(|_| "hang".into())
}

fn in_child(words: &[String]) -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "error: no executable".into();
    };
    let child = std::process::Command::new(exe)
        .args(["--launchd", "probe-child"])
        .args(words)
        .stdout(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return format!("error: {error}"),
    };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < LIMIT => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return "hang".into();
            }
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
    }
    out.trim().to_owned()
}

fn run(words: &[String]) -> String {
    match words.first().map(String::as_str) {
        Some("facts") => facts(),
        Some("post") => {
            let method = words.get(1).map(String::as_str).unwrap_or("hid");
            let keys = words.get(2).map(String::as_str).unwrap_or("a");
            match post(method, keys) {
                Ok(()) => format!("ok ({method}, {keys})"),
                Err(error) => format!("error: {error}"),
            }
        }
        _ => "usage: facts | post <method> <keys>".into(),
    }
}

fn facts() -> String {
    let session = procinfo(std::process::id());
    // SAFETY: plain C calls; the dictionary is released after its description is copied.
    let (dictionary, ax, post, listen, capture) = unsafe {
        let dictionary = CGSessionCopyCurrentDictionary();
        let text = if dictionary.is_null() {
            "none".into()
        } else {
            let text = describe(dictionary);
            CFRelease(dictionary);
            text
        };
        (
            text,
            AXIsProcessTrusted(),
            CGPreflightPostEventAccess(),
            CGPreflightListenEventAccess(),
            CGPreflightScreenCaptureAccess(),
        )
    };
    format!(
        "pid {} uid {} euid {} {session}; ax {ax} postEvent {post} listenEvent {listen} \
         screenCapture {capture}; session {}",
        std::process::id(),
        unsafe { libc::getuid() },
        unsafe { libc::geteuid() },
        dictionary.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// CFCopyDescription as a Rust string.
unsafe fn describe(object: *const c_void) -> String {
    let text = CFCopyDescription(object);
    if text.is_null() {
        return "?".into();
    }
    let mut buffer = vec![0 as c_char; 8192];
    let ok = CFStringGetCString(text, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100);
    CFRelease(text);
    if ok {
        CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
    } else {
        "?".into()
    }
}

fn post(method: &str, keys: &str) -> Result<(), String> {
    let codes = keys.chars().map(|key| if key == 'b' { KEY_DELETE } else { KEY_A });
    match method {
        "hid" => codes.for_each(|code| cg_key(HID_EVENT_TAP, code)),
        "session" => codes.for_each(|code| cg_key(SESSION_EVENT_TAP, code)),
        "app-hid" => {
            accessory_app();
            codes.for_each(|code| cg_key(HID_EVENT_TAP, code));
        }
        "iohid" => {
            let connect = hid_system()?;
            for code in codes {
                hid_key(connect, code)?;
            }
        }
        other => return Err(format!("unknown method {other}")),
    }
    Ok(())
}

fn cg_key(tap: u32, code: u16) {
    for down in [true, false] {
        // SAFETY: a NULL source is allowed; the event is released after posting.
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), code, down);
            if !event.is_null() {
                CGEventPost(tap, event);
                CFRelease(event);
            }
        }
    }
}

fn accessory_app() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(main) = objc2::MainThreadMarker::new() else {
        // Off the main thread (a `self` step): AppKit setup only works in a child step.
        return;
    };
    let app = NSApplication::sharedApplication(main);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

fn hid_system() -> Result<u32, String> {
    // SAFETY: IOKit lookups by name; the service is released once opened.
    unsafe {
        let service = IOServiceGetMatchingService(
            kIOMasterPortDefault,
            IOServiceMatching(c"IOHIDSystem".as_ptr()),
        );
        if service == 0 {
            return Err("no IOHIDSystem".into());
        }
        let mut connect = 0;
        // kIOHIDParamConnectType
        let status = IOServiceOpen(service, mach_task_self_, 1, &mut connect);
        IOObjectRelease(service);
        if status != 0 {
            return Err(format!("IOServiceOpen: {status:#x}"));
        }
        Ok(connect)
    }
}

fn hid_key(connect: u32, code: u16) -> Result<(), String> {
    // NXEventData's key member: origCharSet, repeat, charSet, charCode, keyCode, origCharCode,
    // then reserved words, in a union padded well past its size here.
    let mut data = [0u8; 128];
    data[8..10].copy_from_slice(&code.to_ne_bytes());
    for kind in [10u32, 11] {
        // NX_KEYDOWN, NX_KEYUP; kNXEventDataVersion is 2.
        // SAFETY: data outlives the call and is larger than NXEventData.
        let status = unsafe {
            IOHIDPostEvent(
                connect,
                kind,
                IOGPoint { x: 0, y: 0 },
                data.as_ptr().cast(),
                2,
                0,
                0,
            )
        };
        if status != 0 {
            return Err(format!("IOHIDPostEvent: {status:#x}"));
        }
    }
    Ok(())
}

/// The audit session and launchd domain lines of `launchctl procinfo` (needs root).
fn procinfo(pid: u32) -> String {
    let out = std::process::Command::new("/bin/launchctl")
        .args(["procinfo", &pid.to_string()])
        .output();
    let Ok(out) = out else {
        return "procinfo ?".into();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("session id")
                || line.starts_with("domain =")
                || line.starts_with("responsible")
                || line.starts_with("uid")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

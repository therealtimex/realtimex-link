//! Spike S2b (realtimex-link#11, Addendum 8), removed before Review: which capture works at the
//! lock screen. `rtx-control --capture-probe <dir>`, started by `rtx-link --launchd
//! probe-capture` so TCC credits RealTimeX Link. Every step runs on its own thread with a time
//! limit; each result is one line in `<dir>/probe.log`.

use std::ffi::c_void;
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(8);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayCreateImage(display: u32) -> *const c_void;
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGImageGetDataProvider(image: *const c_void) -> *const c_void;
    fn CGDataProviderCopyData(provider: *const c_void) -> *const c_void;
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
    fn CGDisplayStreamCreateWithDispatchQueue(
        display: u32,
        width: usize,
        height: usize,
        format: i32,
        properties: *const c_void,
        queue: *const c_void,
        handler: *const c_void,
    ) -> *const c_void;
    fn CGDisplayStreamStart(stream: *const c_void) -> i32;
    fn CGDisplayStreamStop(stream: *const c_void) -> i32;
    fn CGEventSourceCreate(state: i32) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFCopyDescription(object: *const c_void) -> *const c_void;
    fn CFStringGetCString(text: *const c_void, buffer: *mut i8, size: isize, encoding: u32) -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, kind: u32, id: *mut u32) -> i32;
}

extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *const c_void;
}

fn description(object: *const c_void) -> String {
    if object.is_null() {
        return "null".into();
    }
    // SAFETY: CF calls on an object the caller owns; the buffer's size is passed.
    unsafe {
        let text = CFCopyDescription(object);
        let mut buffer = vec![0i8; 8192];
        let ok = CFStringGetCString(text, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100);
        CFRelease(text);
        if ok {
            std::ffi::CStr::from_ptr(buffer.as_ptr()).to_string_lossy().split_whitespace().collect::<Vec<_>>().join(" ")
        } else {
            "?".into()
        }
    }
}

/// Run `work` on its own thread; its answer, or "timeout" after LIMIT (the thread is left).
fn timed(work: impl FnOnce() -> String + Send + 'static) -> String {
    let (send, receive) = mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let _ = send.send(work());
    });
    match receive.recv_timeout(LIMIT) {
        Ok(answer) => format!("{answer} ({} ms)", started.elapsed().as_millis()),
        Err(_) => format!("TIMEOUT after {} s", LIMIT.as_secs()),
    }
}

/// How dark a buffer is: the mean of a sample of its bytes (0 is black).
fn mean(bytes: &[u8]) -> u64 {
    let step = (bytes.len() / 4096).max(1);
    let sample: Vec<u64> = bytes.iter().step_by(step).map(|&b| u64::from(b)).collect();
    sample.iter().sum::<u64>() / sample.len().max(1) as u64
}

fn sck_list() -> String {
    match rtx_screens::macos::shareable_content_fresh() {
        Ok((windows, displays)) => format!("ok: {} windows, {} displays", windows.len(), displays.len()),
        Err(error) => format!("error: {error:#}"),
    }
}

fn sck_shot(dir: std::path::PathBuf, name: &'static str) -> String {
    match rtx_screens::capture_display(1280) {
        Ok(shot) => {
            let _ = std::fs::write(dir.join(format!("{name}.jpg")), &shot.jpeg);
            format!("ok: {}x{}, {} jpeg bytes", shot.width, shot.height, shot.jpeg.len())
        }
        Err(error) => format!("error: {error:#}"),
    }
}

fn cg_image() -> String {
    // SAFETY: CG calls on an image this function owns and releases.
    unsafe {
        let image = CGDisplayCreateImage(CGMainDisplayID());
        if image.is_null() {
            return "null image".into();
        }
        let (width, height) = (CGImageGetWidth(image), CGImageGetHeight(image));
        let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
        let brightness = if data.is_null() {
            0
        } else {
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
            let m = mean(bytes);
            CFRelease(data);
            m
        };
        CFRelease(image);
        format!("ok: {width}x{height}, mean byte {brightness}")
    }
}

fn cg_stream() -> String {
    use block2::RcBlock;
    let (send, receive) = mpsc::channel::<i32>();
    let send = std::sync::Mutex::new(send);
    let handler = RcBlock::new(move |status: i32, _time: u64, _surface: *const c_void, _update: *const c_void| {
        let _ = send.lock().map(|send| send.send(status));
    });
    // SAFETY: a stream made, started, stopped and released here; the block outlives it.
    unsafe {
        let queue = dispatch_get_global_queue(0, 0);
        // 'BGRA'
        let stream = CGDisplayStreamCreateWithDispatchQueue(
            CGMainDisplayID(),
            640,
            400,
            0x4247_5241,
            std::ptr::null(),
            queue,
            RcBlock::as_ptr(&handler).cast(),
        );
        if stream.is_null() {
            return "null stream".into();
        }
        let start = CGDisplayStreamStart(stream);
        let mut statuses = Vec::new();
        let until = Instant::now() + Duration::from_secs(4);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match receive.recv_timeout(left) {
                Ok(status) => {
                    statuses.push(status);
                    // kCGDisplayStreamFrameStatusFrameComplete
                    if status == 0 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        CGDisplayStreamStop(stream);
        CFRelease(stream);
        format!("start {start}, statuses {statuses:?}")
    }
}

pub fn run(dir: &str) -> ! {
    let dir = std::path::PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("probe.log")).ok();
    let mut line = |text: String| {
        if let Some(log) = log.as_mut() {
            let _ = writeln!(log, "{text}");
        }
    };
    // SAFETY: plain calls; the dictionary is released after its description.
    line(format!("session: {}", timed(|| unsafe {
        let d = CGSessionCopyCurrentDictionary();
        let text = description(d);
        if !d.is_null() {
            CFRelease(d);
        }
        text
    })));
    line(format!("ax: {}", timed(|| format!("{}", unsafe { AXIsProcessTrusted() }))));
    line(format!("event source private: {}", timed(|| unsafe {
        let s = CGEventSourceCreate(-1);
        let made = !s.is_null();
        if made {
            CFRelease(s);
        }
        format!("{made}")
    })));
    for round in ["plain", "user-activity"] {
        if round == "user-activity" {
            let name = c"RealTimeX Link S2b";
            // SAFETY: a CFString made for the call; kIOPMUserActiveLocal = 0.
            let made = unsafe {
                #[link(name = "CoreFoundation", kind = "framework")]
                extern "C" {
                    fn CFStringCreateWithCString(a: *const c_void, s: *const i8, e: u32) -> *const c_void;
                }
                let text = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), 0x0800_0100);
                let mut id = 0u32;
                IOPMAssertionDeclareUserActivity(text, 0, &mut id)
            };
            line(format!("user activity assertion: {made}"));
            std::thread::sleep(Duration::from_secs(2));
        }
        line(format!("[{round}] sck list: {}", timed(sck_list)));
        let shot_dir = dir.clone();
        let name: &'static str = if round == "plain" { "sck-plain" } else { "sck-activity" };
        line(format!("[{round}] sck screenshot: {}", timed(move || sck_shot(shot_dir, name))));
        line(format!("[{round}] CGDisplayCreateImage: {}", timed(cg_image)));
        line(format!("[{round}] CGDisplayStream: {}", timed(cg_stream)));
    }
    line("done".into());
    std::process::exit(0);
}

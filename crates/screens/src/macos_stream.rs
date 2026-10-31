//! A display as one continuous ScreenCaptureKit stream, for live views (#2683): the main one, or
//! the one a live view chose (realtimex-link#12).
//!
//! A one-shot screenshot per frame made macOS re-check the Screen Recording permission (tccd),
//! redraw the recording indicator (systemstatusd, ControlCenter), verify our signature (trustd)
//! and set a capture up and down, 10 times a second: more than a core on a Mac mini. One stream
//! does that once, scales in ScreenCaptureKit and delivers a frame only when the screen changes.
//! It starts with the first frame a live view asks for and stops once none has asked for a while.
//! When macOS stops it (an error, the display going away), the next request starts a new one, so
//! a live view recovers instead of showing the last picture forever.

use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};

use crate::Frame;

/// 'BGRA': what the live view's encoder path takes.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;
/// At most this many frames a second: the live view asks for 15 (peer.rs DEFAULT_FPS).
const MAX_FPS: i32 = 15;
/// A stream no live view has asked a frame of for this long stops.
const IDLE_STOP: Duration = Duration::from_secs(5);
/// How long the first frame of a new stream may take.
const FIRST_FRAME: Duration = Duration::from_secs(3);
/// How long a live view's width is taken to fit the same stream without listing the displays
/// again: a display that changes resolution gets a stream of its new size this soon.
const REFIT: Duration = Duration::from_secs(30);

/// The newest frame, and why macOS stopped the stream if it did.
#[derive(Default)]
struct Latest {
    frame: Mutex<Option<Frame>>,
    stopped: Mutex<Option<String>>,
    arrived: Condvar,
}

impl Latest {
    fn stop(&self, why: String) {
        *self.stopped.lock().unwrap() = Some(why);
        // Wake a request waiting for a first frame; it sees the stop.
        let _guard = self.frame.lock().unwrap();
        self.arrived.notify_all();
    }

    fn stopped(&self) -> Option<String> {
        self.stopped.lock().unwrap().clone()
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RtxCaptureLiveViewOutput"]
    #[ivars = Arc<Latest>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Status frames (the screen did not change) carry no picture.
            let Some(frame) = (unsafe { bgra(sample) }) else {
                return;
            };
            let latest = self.ivars();
            let mut slot = latest.frame.lock().unwrap();
            *slot = Some(Frame {
                seq: crate::next_frame_seq(),
                ..frame
            });
            latest.arrived.notify_all();
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().stop(error.localizedDescription().to_string());
        }
    }
);

impl Output {
    fn new(latest: Arc<Latest>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(latest);
        unsafe { msg_send![super(this), init] }
    }
}

/// The picture in `sample` as packed BGRA rows, or None when it has none.
unsafe fn bgra(sample: &CMSampleBuffer) -> Option<Frame> {
    let pixels = sample.image_buffer()?;
    if CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) != 0 {
        return None;
    }
    let (width, height) = (
        CVPixelBufferGetWidth(&pixels),
        CVPixelBufferGetHeight(&pixels),
    );
    let stride = CVPixelBufferGetBytesPerRow(&pixels);
    let base = CVPixelBufferGetBaseAddress(&pixels) as *const u8;
    let frame = (!base.is_null() && stride >= width * 4).then(|| {
        let mut data = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            data.extend_from_slice(std::slice::from_raw_parts(
                base.add(row * stride),
                width * 4,
            ));
        }
        Frame {
            data: data.into(),
            width: width as u32,
            height: height as u32,
            seq: 0,
        }
    });
    CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
    frame
}

struct Running {
    stream: Retained<SCStream>,
    // Kept alive while the stream may call it.
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    latest: Arc<Latest>,
    /// The display it shows (CGDirectDisplayID).
    display: u32,
    size: (usize, usize),
    /// The live view widths found to fit `size`, and when: a frame request of one of them
    /// finds this stream without listing the displays (see [`known_stream`]).
    widths: Vec<(u32, Instant)>,
    last_asked: Instant,
}

/// The running streams, one per display and size so live views of different displays and widths
/// each keep theirs, and whether the idle reaper runs (one at a time).
struct Streams {
    running: Vec<Running>,
    reaper: bool,
}

static STREAMS: Mutex<Streams> = Mutex::new(Streams {
    running: Vec::new(),
    reaper: false,
});
/// At most this many displays and sizes stream at once; another one replaces the one asked least
/// recently.
const MAX_STREAMS: usize = 4;

/// The newest frame of `display` (None: the main display) at most `max_width` wide, from the
/// running stream (started on first use, and again after a size change or after macOS stopped it).
/// A new `seq` only when the screen changed.
pub fn display_frame(display: Option<u32>, max_width: u32) -> Result<Frame> {
    // The main display's id costs nothing to ask (no gate), so a known stream is found without
    // listing the displays.
    let key = display.unwrap_or_else(|| objc2_core_graphics::CGMainDisplayID());
    let latest = match known_stream(key, max_width) {
        Some(latest) => latest,
        None => stream_for(display, max_width)?,
    };
    let slot = latest.frame.lock().unwrap();
    let (slot, _) = latest
        .arrived
        .wait_timeout_while(slot, FIRST_FRAME, |frame| {
            frame.is_none() && latest.stopped.lock().unwrap().is_none()
        })
        .unwrap();
    let frame = slot.clone();
    drop(slot);
    if let Some(why) = latest.stopped() {
        // The next request starts a new stream.
        bail!("the display stream stopped: {why}");
    }
    frame.ok_or_else(|| anyhow!("the display stream sent no picture"))
}

/// The running stream of `display` a request of `max_width` was fitted to within REFIT, unless
/// macOS stopped it. Found without listing the displays: a live view asks for 15 frames a second, and
/// listing goes through the one-request gate (`crate::gate`), where it would queue behind
/// screenshots and, after a stalled request, be refused, although the stream itself still runs.
fn known_stream(display: u32, max_width: u32) -> Option<Arc<Latest>> {
    let mut streams = STREAMS.lock().unwrap();
    let running = streams.running.iter_mut().find(|r| {
        r.display == display
            && r.widths
            .iter()
            .any(|&(w, at)| w == max_width && at.elapsed() < REFIT)
            && r.latest.stopped().is_none()
    })?;
    running.last_asked = Instant::now();
    Some(running.latest.clone())
}

/// The stream for `display` and `max_width`, found by the size the display fits to now (which
/// lists the displays): the running one of that display and size, or a new one.
fn stream_for(display: Option<u32>, max_width: u32) -> Result<Arc<Latest>> {
    let (filter, display) = crate::macos::display_filter(display)?;
    let size = crate::macos::fit_filter(&filter, max_width)?;
    let mut streams = STREAMS.lock().unwrap();
    let same = |r: &Running| r.display == display && r.size == size;
    let found = streams.running.iter().position(same);
    if let Some(i) = found.filter(|&i| streams.running[i].latest.stopped().is_some()) {
        stop(streams.running.swap_remove(i));
    }
    let index = match streams.running.iter().position(same) {
        Some(i) => i,
        None => {
            if streams.running.len() >= MAX_STREAMS {
                let oldest = (0..streams.running.len())
                    .min_by_key(|&i| streams.running[i].last_asked)
                    .unwrap();
                stop(streams.running.swap_remove(oldest));
            }
            streams.running.push(start(&filter, display, size)?);
            streams.running.len() - 1
        }
    };
    if !streams.reaper {
        streams.reaper = true;
        thread::spawn(stop_when_idle);
    }
    // This width fits this stream now, and no other of this display (it may have changed size).
    for running in streams.running.iter_mut().filter(|r| r.display == display) {
        running.widths.retain(|&(w, _)| w != max_width);
    }
    let running = &mut streams.running[index];
    running.widths.push((max_width, Instant::now()));
    running.last_asked = Instant::now();
    Ok(running.latest.clone())
}

fn start(filter: &SCContentFilter, display: u32, (width, height): (usize, usize)) -> Result<Running> {
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setShowsCursor(false);
        config.setMinimumFrameInterval(CMTime::new(1, MAX_FPS));
        config.setQueueDepth(3);
    }
    let latest = Arc::new(Latest::default());
    let output = Output::new(latest.clone());
    let queue = DispatchQueue::new("com.realtimex.rtx-capture.live-view", None);
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|e| anyhow!("display stream output: {}", e.localizedDescription()))?;
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let done = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string()));
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) };
    match rx.recv_timeout(FIRST_FRAME) {
        Ok(None) => {}
        Ok(Some(e)) => bail!("display stream did not start: {e}"),
        Err(_) => bail!("display stream did not start in time"),
    }
    Ok(Running {
        stream,
        _output: output,
        _queue: queue,
        latest,
        display,
        size: (width, height),
        widths: Vec::new(),
        last_asked: Instant::now(),
    })
}

fn stop(running: Running) {
    unsafe { running.stream.stopCaptureWithCompletionHandler(None) };
}

/// Stops each stream no live view has asked a frame of in IDLE_STOP; ends with the last one.
fn stop_when_idle() {
    loop {
        thread::sleep(IDLE_STOP / 2);
        let mut streams = STREAMS.lock().unwrap();
        while let Some(i) = streams
            .running
            .iter()
            .position(|r| r.last_asked.elapsed() >= IDLE_STOP)
        {
            stop(streams.running.swap_remove(i));
        }
        if streams.running.is_empty() {
            streams.reaper = false;
            return;
        }
    }
}

// The stream and its output are used from the frame-request thread and stopped from the idle
// thread; ScreenCaptureKit allows that.
unsafe impl Send for Running {}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest_for(size_of: u32) -> Arc<Latest> {
        let (filter, display) = crate::macos::display_filter(None).unwrap();
        let size = crate::macos::fit_filter(&filter, size_of).unwrap();
        let streams = STREAMS.lock().unwrap();
        let running = streams
            .running
            .iter()
            .find(|r| r.display == display && r.size == size)
            .unwrap();
        running.latest.clone()
    }

    /// Live ScreenCaptureKit streams: need Screen Recording for the process running the tests.
    /// `cargo test --release -p rtx-capture live_view_streams -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn live_view_streams_a_stopped_stream_is_replaced_on_the_next_request() {
        let first = display_frame(None, 640).expect("a first frame");
        let before = latest_for(640);
        // What the delegate does when macOS stops the stream.
        before.stop("injected stop".into());
        let again = display_frame(None, 640).expect("a frame from a new stream");
        let after = latest_for(640);
        assert!(!Arc::ptr_eq(&before, &after), "the stopped stream was kept");
        assert!(after.stopped().is_none());
        assert!(again.seq > first.seq, "{} after {}", again.seq, first.seq);
        assert!(STREAMS.lock().unwrap().reaper);
    }

    /// Two live views of different widths keep one stream each, asked alternately.
    #[test]
    #[ignore]
    fn live_view_streams_alternating_sizes_keep_their_streams() {
        display_frame(None, 640).unwrap();
        display_frame(None, 1280).unwrap();
        let (small, large) = (latest_for(640), latest_for(1280));
        for _ in 0..6 {
            display_frame(None, 640).unwrap();
            display_frame(None, 1280).unwrap();
        }
        assert!(
            Arc::ptr_eq(&small, &latest_for(640)),
            "the 640 stream was recreated"
        );
        assert!(
            Arc::ptr_eq(&large, &latest_for(1280)),
            "the 1280 stream was recreated"
        );
        // A third size streams too: up to MAX_STREAMS keep theirs.
        display_frame(None, 1280).unwrap();
        display_frame(None, 960).unwrap();
        let streams = STREAMS.lock().unwrap();
        assert_eq!(streams.running.len(), 3.min(MAX_STREAMS));
        assert!(streams.reaper);
        drop(streams);
        assert!(Arc::ptr_eq(&large, &latest_for(1280)));
    }
}

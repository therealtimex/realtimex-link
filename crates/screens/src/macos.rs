//! Window screenshots through ScreenCaptureKit's single-window filter: the window's own content,
//! even when other windows cover it (verified in the #2453 spike; a display crop would show
//! whatever covers the window). Needs the Screen Recording permission of the launching app.
//!
//! Every screenshot is also read with Vision's on-device text recognition (`core::window_text`
//! decides what the text means): it tells when the user presents, which makes the window useless,
//! and then the display is shot instead.

use std::ptr::NonNull;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use crate::image::{THUMB_HEIGHT, THUMB_WIDTH};
use crate::text::TextLine;
use crate::window::WindowInfo;
use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, AllocAnyThread};
use objc2_core_foundation::{
    CFDictionary, CFMutableData, CFRetained, CFString, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString};
use objc2_image_io::{kCGImageDestinationLossyCompressionQuality, CGImageDestination};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRequest,
    VNRequestTextRecognitionLevel,
};

use crate::Shot;

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

/// A ScreenCaptureKit request that hasn't answered by then has stalled. In a 2-hour Zoom meeting
/// (dogfood run 7), 9% of requests at a 5 s interval never answered within 10 s while the others
/// took about 0.1 s; a shorter wait loses less of the next interval.
const TIMEOUT: Duration = Duration::from_secs(5);
/// A window list this recent is reused by the screenshot that follows it, so a tick asks
/// ScreenCaptureKit twice (list, capture) instead of three times.
const REUSE_LIST: Duration = Duration::from_secs(2);
const JPEG_QUALITY: f64 = 0.75;

/// Window-filter capture asserts in CoreGraphics (`CGS_REQUIRE_INIT`) unless AppKit has a
/// window-server connection. Call once, on the main thread, before taking screenshots.
pub fn init_window_server() {
    if let Some(class) = AnyClass::get(c"NSApplication") {
        let app: *mut AnyObject = unsafe { msg_send![class, sharedApplication] };
        // The helpers sit in RealTimeX.AI.app/Contents/MacOS, so AppKit takes them for the app
        // itself and gives them a second Dock icon (#2747): they are accessories, without one.
        const ACCESSORY: isize = 1; // NSApplicationActivationPolicyAccessory
        if !app.is_null() {
            let _: bool = unsafe { msg_send![app, setActivationPolicy: ACCESSORY] };
        }
    }
}

/// SCWindow and SCDisplay objects are immutable descriptions; moving them across threads is fine.
#[derive(Clone)]
struct SendContent(Vec<Retained<SCWindow>>, Vec<Retained<SCDisplay>>);
unsafe impl Send for SendContent {}

static LAST_LIST: Mutex<Option<(Instant, SendContent)>> = Mutex::new(None);

/// The last window list if it is recent enough, else a new one.
fn recent_content() -> Result<SendContent> {
    if let Some((at, content)) = LAST_LIST.lock().unwrap().as_ref() {
        if at.elapsed() < REUSE_LIST {
            return Ok(content.clone());
        }
    }
    shareable_content()
}

fn error_text(err: *mut NSError) -> String {
    unsafe { err.as_ref() }
        .map(|e| e.localizedDescription().to_string())
        .unwrap_or_else(|| "unknown error".into())
}

/// SCScreenshotManager exists from macOS 14; ScreenCaptureKit itself is weak-linked (build.rs).
pub fn supported() -> bool {
    AnyClass::get(c"SCScreenshotManager").is_some()
}

/// Shareable windows and displays.
pub type Content = (Vec<Retained<SCWindow>>, Vec<Retained<SCDisplay>>);

/// A fresh list of shareable windows and displays (window video binds to the answer, so no reuse).
pub fn shareable_content_fresh() -> Result<Content> {
    let c = shareable_content()?;
    Ok((c.0, c.1))
}

fn shareable_windows() -> Result<Vec<Retained<SCWindow>>> {
    Ok(shareable_content()?.0)
}

fn shareable_content() -> Result<SendContent> {
    if !supported() {
        bail!("window screenshots need macOS 14 or later");
    }
    let (tx, rx) = mpsc::channel::<Result<SendContent, String>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let result = match unsafe { content.as_ref() } {
            Some(c) => Ok(SendContent(
                unsafe { c.windows() }.to_vec(),
                unsafe { c.displays() }.to_vec(),
            )),
            None => Err(error_text(err)),
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &block,
        )
    };
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(content)) => {
            *LAST_LIST.lock().unwrap() = Some((Instant::now(), content.clone()));
            Ok(content)
        }
        // Transient too: "The user declined TCCs" once in dogfood run 7, with the permission on.
        Ok(Err(e)) => bail!(
            "cannot list windows ({e}); if this persists, allow Screen Recording for the app that started rtx-capture"
        ),
        Err(_) => bail!("listing windows timed out"),
    }
}

/// Every window of these processes, at any layer and size, on screen or not, one line each: for
/// the session log when the meeting window disappears (what does the app show instead?).
pub fn window_dump(pids: &[i32]) -> Vec<String> {
    let Ok(windows) = shareable_windows() else {
        return vec![];
    };
    windows
        .iter()
        .filter_map(|w| {
            let app = unsafe { w.owningApplication() }?;
            let pid = unsafe { app.processID() };
            if !pids.contains(&pid) {
                return None;
            }
            let frame = unsafe { w.frame() };
            Some(format!(
                "{} pid {pid} {} layer {} {} {}x{} at {},{} {:?}",
                unsafe { w.windowID() },
                unsafe { app.bundleIdentifier() },
                unsafe { w.windowLayer() },
                if unsafe { w.isOnScreen() } {
                    "on-screen"
                } else {
                    "off-screen"
                },
                frame.size.width,
                frame.size.height,
                frame.origin.x,
                frame.origin.y,
                unsafe { w.title() }
                    .map(|t| t.to_string())
                    .unwrap_or_default(),
            ))
        })
        .collect()
}

/// App windows big enough to matter: normal ones (layer 0), and floating ones (above normal,
/// below the menu bar, e.g. Meet's picture-in-picture at layer 3), marked `floating`.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let mut out = vec![];
    for w in shareable_windows()? {
        let (layer, frame) = unsafe { (w.windowLayer(), w.frame()) };
        if !(0..=20).contains(&layer) || frame.size.width < 200.0 || frame.size.height < 150.0 {
            continue;
        }
        let app = unsafe { w.owningApplication() };
        out.push(WindowInfo {
            id: unsafe { w.windowID() },
            pid: app.as_ref().map(|a| unsafe { a.processID() }).unwrap_or(-1),
            app: app
                .as_ref()
                .map(|a| unsafe { a.bundleIdentifier() }.to_string())
                .unwrap_or_default(),
            app_name: app
                .as_ref()
                .map(|a| unsafe { a.applicationName() }.to_string())
                .unwrap_or_default(),
            title: unsafe { w.title() }
                .map(|t| t.to_string())
                .unwrap_or_default(),
            on_screen: unsafe { w.isOnScreen() },
            frame: [
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ],
            floating: layer != 0,
            layer: None,
            unselectable_reason: None,
        });
    }
    Ok(out)
}

fn encode_jpeg(image: &CGImage) -> Result<Vec<u8>> {
    let data = CFMutableData::new(None, 0).context("CFMutableData")?;
    let dest = unsafe {
        CGImageDestination::with_data(&data, &CFString::from_static_str("public.jpeg"), 1, None)
    }
    .context("JPEG encoder")?;
    // NSDictionary/NSString/NSNumber are toll-free bridged to their CF counterparts.
    let key: &NSString = unsafe {
        &*(kCGImageDestinationLossyCompressionQuality as *const CFString as *const NSString)
    };
    let props = NSDictionary::from_retained_objects(&[key], &[NSNumber::new_f64(JPEG_QUALITY)]);
    let props_cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&props) as *const CFDictionary) };
    unsafe {
        dest.add_image(image, Some(props_cf));
        if !dest.finalize() {
            bail!("JPEG encoding failed");
        }
    }
    Ok(data.to_vec())
}

/// 64x36 grayscale, for change detection.
fn thumbnail(image: &CGImage) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; THUMB_WIDTH * THUMB_HEIGHT];
    let space = CGColorSpace::new_device_gray().context("gray color space")?;
    let ctx = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            THUMB_WIDTH,
            THUMB_HEIGHT,
            8,
            THUMB_WIDTH,
            Some(&space),
            CGImageAlphaInfo::None.0,
        )
    }
    .context("bitmap context")?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(THUMB_WIDTH as f64, THUMB_HEIGHT as f64),
    );
    CGContext::draw_image(Some(&ctx), rect, Some(image));
    drop(ctx);
    Ok(pixels)
}

/// Lines of text in the image, recognized on-device (Vision); empty if recognition fails.
fn recognize_text(image: &CGImage) -> Vec<TextLine> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(false);
    request.setAutomaticallyDetectsLanguage(true);
    let options = NSDictionary::<VNImageOption, AnyObject>::new();
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &options,
        )
    };
    let as_request: Retained<VNRequest> =
        Retained::into_super(Retained::into_super(request.clone()));
    if handler
        .performRequests_error(&NSArray::from_retained_slice(&[as_request]))
        .is_err()
    {
        return vec![];
    }
    let Some(results) = request.results() else {
        return vec![];
    };
    results
        .iter()
        .filter_map(|observation| {
            let best = observation.topCandidates(1).firstObject()?;
            // Vision boxes are normalized with the origin at the bottom left.
            let b = unsafe { observation.boundingBox() };
            Some(TextLine::new(
                &best.string().to_string(),
                best.confidence(),
                [
                    b.origin.x as f32,
                    (1.0 - b.origin.y - b.size.height) as f32,
                    b.size.width as f32,
                    b.size.height as f32,
                ],
            ))
        })
        .collect()
}

/// One screenshot through `filter`, at most `max_width` pixels wide, with its text.
fn shoot(filter: &SCContentFilter, max_width: u32, what: &str, title: String) -> Result<Shot> {
    let image = capture_image(filter, max_width, what)?;
    Ok(Shot {
        jpeg: encode_jpeg(&image)?,
        thumb: thumbnail(&image)?,
        width: CGImage::width(Some(&image)) as u32,
        height: CGImage::height(Some(&image)) as u32,
        title,
        text: recognize_text(&image),
    })
}

/// The picture through `filter`, at most `max_width` pixels wide (scaled by ScreenCaptureKit).
fn capture_image(
    filter: &SCContentFilter,
    max_width: u32,
    what: &str,
) -> Result<CFRetained<CGImage>> {
    let (width, height) = fit(filter, max_width, what)?;
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setShowsCursor(false);
    }

    let (tx, rx) = mpsc::channel::<Result<CFRetained<CGImage>, String>>();
    let block = RcBlock::new(move |image: *mut CGImage, err: *mut NSError| {
        let result = match NonNull::new(image) {
            Some(p) => Ok(unsafe { CFRetained::retain(p) }),
            None => Err(error_text(err)),
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            &config,
            Some(&block),
        )
    };
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(image)) => Ok(image),
        Ok(Err(e)) => bail!("screenshot of {what} failed: {e}"),
        Err(_) => bail!("screenshot of {what} timed out"),
    }
}

/// The size of `filter`'s picture scaled to at most `max_width` pixels wide.
fn fit(filter: &SCContentFilter, max_width: u32, what: &str) -> Result<(usize, usize)> {
    let (scale, rect) = unsafe { (filter.pointPixelScale() as f64, filter.contentRect()) };
    let (pw, ph) = (rect.size.width * scale, rect.size.height * scale);
    if pw < 1.0 || ph < 1.0 {
        bail!("{what} has no visible size");
    }
    let k = (max_width as f64 / pw).min(1.0);
    Ok(((pw * k).round() as usize, (ph * k).round() as usize))
}

/// Like [`fit`], with even sides for the 4:2:0 encoder.
pub(crate) fn fit_filter(filter: &SCContentFilter, max_width: u32) -> Result<(usize, usize)> {
    let (width, height) = fit(filter, max_width, "the display")?;
    Ok(((width & !1).max(2), (height & !1).max(2)))
}

/// Screenshot one window at most `max_width` pixels wide.
pub fn capture_window(id: u32, max_width: u32) -> Result<Shot> {
    let window = recent_content()?
        .0
        .into_iter()
        .find(|w| unsafe { w.windowID() } == id)
        .ok_or_else(|| anyhow!("window {id} is gone"))?;
    let title = unsafe { window.title() }
        .map(|t| t.to_string())
        .unwrap_or_default();
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
    };
    shoot(&filter, max_width, &format!("window {id}"), title)
}

/// Screenshot the main display (what a presenting user most often shares), at most `max_width`
/// pixels wide.
pub fn capture_display(max_width: u32) -> Result<Shot> {
    let filter = main_display_filter()?;
    shoot(&filter, max_width, "the display", "display".into())
}

/// The main display, else the first one.
pub(crate) fn main_display_filter() -> Result<Retained<SCContentFilter>> {
    let displays = recent_content()?.1;
    let main = CGMainDisplayID();
    let display = displays
        .iter()
        .find(|d| unsafe { d.displayID() } == main)
        .or(displays.first())
        .ok_or_else(|| anyhow!("no display to screenshot"))?;
    Ok(unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::new(),
        )
    })
}

/// The same picture as [`capture_display`], as a live-view video frame: from one continuous
/// stream, not a screenshot per frame (#2683).
pub fn capture_display_frame(max_width: u32) -> Result<crate::Frame> {
    crate::macos_stream::display_frame(max_width)
}

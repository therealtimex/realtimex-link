//! The lock screen's picture (realtimex-link#11, Addendum 8 / S2b): while the owner's session is
//! locked, ScreenCaptureKit gives a blank picture, and CGDisplayCreateImage shows the lock
//! screen. Only then is it used; ScreenCaptureKit stays the normal path.

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{bail, Result};
use rtx_screens::image::{encode_jpeg_bgra, fit_width, scale_bgra, thumbnail_bgra, Bgra};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayCreateImage(display: u32) -> *const c_void;
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGImageGetBytesPerRow(image: *const c_void) -> usize;
    fn CGImageGetBitsPerPixel(image: *const c_void) -> usize;
    fn CGImageGetDataProvider(image: *const c_void) -> *const c_void;
    fn CGDataProviderCopyData(provider: *const c_void) -> *const c_void;
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
    fn CFStringCreateWithCString(allocator: *const c_void, text: *const i8, encoding: u32) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut c_void) -> bool;
}

/// Whether this session's screen is locked (`CGSSessionScreenIsLocked`, present only then).
pub fn screen_locked() -> bool {
    // SAFETY: CF calls on a dictionary and key this function owns and releases.
    unsafe {
        let dictionary = CGSessionCopyCurrentDictionary();
        if dictionary.is_null() {
            return false;
        }
        let key = CFStringCreateWithCString(std::ptr::null(), c"CGSSessionScreenIsLocked".as_ptr(), 0x0800_0100);
        let value = CFDictionaryGetValue(dictionary, key);
        let mut locked: i32 = 0;
        // kCFNumberSInt32Type; a CFBoolean reads the same way.
        let read = !value.is_null() && CFNumberGetValue(value, 3, (&mut locked as *mut i32).cast());
        CFRelease(key);
        CFRelease(dictionary);
        read && locked != 0
    }
}

/// The CoreGraphics id of `display` (None: the main display); one that isn't attached is
/// `DisplayNotFound` (realtimex-link#12).
fn display_id(display: Option<&str>) -> Result<u32> {
    let Some(id) = display else {
        // SAFETY: a plain getter.
        return Ok(unsafe { CGMainDisplayID() });
    };
    // CoreGraphics only: this runs on the capture worker, where AppKit (the names) isn't used.
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    // SAFETY: the buffer holds `ids.len()` ids and CoreGraphics writes at most that many.
    let listed = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } == 0;
    let attached = &ids[..count.min(ids.len() as u32) as usize];
    match id.parse::<u32>() {
        Ok(number) if listed && attached.contains(&number) => Ok(number),
        _ => Err(rtx_screens::DisplayNotFound(id.to_string()).into()),
    }
}

/// The picture of `display` (None: the main display) as packed BGRA rows, through
/// CGDisplayCreateImage.
fn display_bgra<T>(display: Option<&str>, with: impl FnOnce(&Bgra) -> Result<T>) -> Result<T> {
    let id = display_id(display)?;
    // SAFETY: CG and CF objects made and released here; the bytes are read while the data lives.
    unsafe {
        let image = CGDisplayCreateImage(id);
        if image.is_null() {
            bail!("the display can't be captured");
        }
        let (width, height, stride) = (CGImageGetWidth(image), CGImageGetHeight(image), CGImageGetBytesPerRow(image));
        if CGImageGetBitsPerPixel(image) != 32 {
            CFRelease(image);
            bail!("the display's picture isn't 32-bit");
        }
        let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
        if data.is_null() {
            CFRelease(image);
            bail!("the display's picture has no data");
        }
        let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
        let result = if bytes.len() < stride * height {
            Err(anyhow::anyhow!("the display's picture is short"))
        } else {
            with(&Bgra { width, height, stride, data: bytes })
        };
        CFRelease(data);
        CFRelease(image);
        result
    }
}

/// A screenshot of the lock screen.
pub fn screenshot(display: Option<&str>, max_width: u32) -> Result<super::super::Shot> {
    display_bgra(display, |image| {
        let (jpeg, width, height) = encode_jpeg_bgra(image, max_width as usize)?;
        Ok(super::super::Shot {
            jpeg,
            thumb: thumbnail_bgra(image),
            width: width as u32,
            height: height as u32,
            title: "display".into(),
            text: Vec::new(),
        })
    })
}

/// A live-view frame of the lock screen.
pub fn frame(display: Option<&str>, max_width: u32) -> Result<super::super::Frame> {
    display_bgra(display, |image| {
        let (width, height) = fit_width(image.width, image.height, max_width as usize);
        let data: Arc<[u8]> = scale_bgra(image, width, height).into();
        Ok(super::super::Frame { data, width: width as u32, height: height as u32, seq: 0 })
    })
}

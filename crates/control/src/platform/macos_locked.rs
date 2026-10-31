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

/// The main display's picture as packed BGRA rows, through CGDisplayCreateImage.
fn display_bgra<T>(with: impl FnOnce(&Bgra) -> Result<T>) -> Result<T> {
    // SAFETY: CG and CF objects made and released here; the bytes are read while the data lives.
    unsafe {
        let image = CGDisplayCreateImage(CGMainDisplayID());
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
pub fn screenshot(max_width: u32) -> Result<super::super::Shot> {
    display_bgra(|image| {
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
pub fn frame(max_width: u32) -> Result<super::super::Frame> {
    display_bgra(|image| {
        let (width, height) = fit_width(image.width, image.height, max_width as usize);
        let data: Arc<[u8]> = scale_bgra(image, width, height).into();
        Ok(super::super::Frame { data, width: width as u32, height: height as u32, seq: 0 })
    })
}

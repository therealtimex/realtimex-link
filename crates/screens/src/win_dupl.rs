//! The primary display through DXGI Desktop Duplication, for live views (#2685).
//!
//! A GDI BitBlt of the whole screen per frame cost about 60% of a core on rta-office-win-01 while
//! watched, changed or not. A duplication hands over a frame only when the desktop changed, copied
//! on the GPU; an unchanged screen returns the last frame with its number, so the encoder skips it.
//! Where duplication is unavailable (an RDP session, the secure desktop, no GPU output), the caller
//! falls back to GDI, and duplication is tried again after RETRY. A rotated (portrait or flipped)
//! display also uses GDI: duplication hands over the unrotated surface, which would show the
//! desktop sideways while clicks follow the real orientation. The duplication is released once no
//! live view has asked for a frame in IDLE_RELEASE.

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::image::Bgra;
use anyhow::{anyhow, bail, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};

use crate::Frame;

/// After duplication failed, use GDI this long before trying it again.
const RETRY: Duration = Duration::from_secs(2);
/// How long the first frame of a new duplication may take.
const FIRST_FRAME_MS: u32 = 500;
/// A duplication no live view has asked a frame of for this long is released.
const IDLE_RELEASE: Duration = Duration::from_secs(5);

struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutputDuplication,
    /// A CPU-readable copy of the desktop texture, made for its size.
    staging: Option<(ID3D11Texture2D, (u32, u32))>,
    /// The last frame and the width it was made for.
    last: Option<(Frame, u32)>,
}

// DXGI and D3D11 objects are free-threaded; this one is only used under the LIVE lock.
unsafe impl Send for Duplication {}

enum Live {
    Running(Box<Duplication>),
    /// Duplication failed; GDI until then.
    Failed(Instant),
}

struct State {
    live: Option<Live>,
    last_asked: Option<Instant>,
    /// The idle reaper runs (one at a time).
    reaper: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    live: None,
    last_asked: None,
    reaper: false,
});

/// The newest frame of the primary display at most `max_width` wide, or None when duplication is
/// unavailable right now (the caller uses GDI). A new `seq` only when the desktop changed.
pub fn display_frame(max_width: u32) -> Option<Frame> {
    let mut state = STATE.lock().unwrap();
    state.last_asked = Some(Instant::now());
    if !state.reaper {
        state.reaper = true;
        thread::spawn(release_when_idle);
    }
    let live = &mut state.live;
    if let Some(Live::Failed(at)) = *live {
        if at.elapsed() < RETRY {
            return None;
        }
        *live = None;
    }
    if live.is_none() {
        *live = Some(match Duplication::open() {
            Ok(d) => Live::Running(Box::new(d)),
            Err(error) => {
                eprintln!("[capture] desktop duplication unavailable, using GDI: {error:#}");
                Live::Failed(Instant::now())
            }
        });
    }
    let Some(Live::Running(duplication)) = live.as_mut() else {
        return None;
    };
    match duplication.frame(max_width) {
        Ok(frame) => Some(frame),
        Err(error) => {
            // Access lost (a resolution change, the secure desktop): open a new one later.
            eprintln!("[capture] desktop duplication stopped: {error:#}");
            *live = Some(Live::Failed(Instant::now()));
            None
        }
    }
}

/// Drops the duplication (and its device and textures) once no live view has asked for a frame in
/// IDLE_RELEASE; ends with it.
fn release_when_idle() {
    loop {
        thread::sleep(IDLE_RELEASE / 2);
        let mut state = STATE.lock().unwrap();
        if state
            .last_asked
            .is_none_or(|at| at.elapsed() >= IDLE_RELEASE)
        {
            state.live = None;
            state.reaper = false;
            return;
        }
    }
}

impl Duplication {
    fn open() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?
        };
        let device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;
        let adapter = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()? };
        // The primary output is the one at the desktop's origin.
        let mut index = 0;
        let output = loop {
            let output = unsafe { adapter.EnumOutputs(index) }
                .map_err(|_| anyhow!("no primary display output on the GPU"))?;
            let desc = unsafe { output.GetDesc()? };
            let origin = desc.DesktopCoordinates;
            if desc.AttachedToDesktop.as_bool() && origin.left == 0 && origin.top == 0 {
                if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                    && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                {
                    bail!(
                        "the display is rotated ({:?}); GDI keeps its orientation",
                        desc.Rotation
                    );
                }
                break output;
            }
            index += 1;
        };
        let output = unsafe { output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)? };
        Ok(Self {
            device,
            context,
            output,
            staging: None,
            last: None,
        })
    }

    fn frame(&mut self, max_width: u32) -> Result<Frame> {
        let wait = if self.last.is_some() {
            0
        } else {
            FIRST_FRAME_MS
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.output.AcquireNextFrame(wait, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                return self.unchanged(max_width);
            }
            Err(e) => return Err(e.into()),
        }
        // The frame must be released whatever happens next.
        let result = (|| {
            // A mouse-only update leaves the desktop image as it was.
            if info.LastPresentTime == 0 && self.last.is_some() {
                return self.unchanged(max_width);
            }
            let texture: ID3D11Texture2D = resource
                .ok_or_else(|| anyhow!("no desktop image"))?
                .cast()?;
            self.read(&texture, max_width)
        })();
        unsafe { self.output.ReleaseFrame()? };
        result
    }

    /// The last frame again, as long as it was made for this width.
    fn unchanged(&mut self, max_width: u32) -> Result<Frame> {
        match &self.last {
            Some((frame, width)) if *width == max_width => Ok(frame.clone()),
            _ => bail!("no picture yet"),
        }
    }

    fn read(&mut self, texture: &ID3D11Texture2D, max_width: u32) -> Result<Frame> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            bail!("unsupported desktop format {:?}", desc.Format);
        }
        let size = (desc.Width, desc.Height);
        if self.staging.as_ref().map(|(_, s)| *s) != Some(size) {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: desc.SampleDesc,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))?
            };
            let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;
            self.staging = Some((staging, size));
        }
        let (staging, _) = self.staging.as_ref().unwrap();
        unsafe { self.context.CopyResource(staging, texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        let (width, height) = (desc.Width as usize, desc.Height as usize);
        let stride = mapped.RowPitch as usize;
        let data =
            unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, stride * height) };
        let frame = Frame {
            seq: crate::next_frame_seq(),
            ..Frame::scaled(
                &Bgra {
                    width,
                    height,
                    stride,
                    data,
                },
                max_width,
            )
        };
        unsafe { self.context.Unmap(staging, 0) };
        self.last = Some((frame.clone(), max_width));
        Ok(frame)
    }
}

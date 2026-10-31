//! A display through DXGI Desktop Duplication, for live views (#2685): the primary one, or the
//! one a live view chose (realtimex-link#12), each with a duplication of its own.
//!
//! A GDI BitBlt of the whole screen per frame cost about 60% of a core on rta-office-win-01 while
//! watched, changed or not. A duplication hands over a frame only when the desktop changed, copied
//! on the GPU; an unchanged screen returns the last frame with its number, so the encoder skips it.
//! Where duplication is unavailable (an RDP session, the secure desktop, no GPU output), the caller
//! falls back to GDI, and duplication is tried again after RETRY. A rotated (portrait or flipped)
//! display also uses GDI: duplication hands over the unrotated surface, which would show the
//! desktop sideways while clicks follow the real orientation. A display's duplication is made on
//! the adapter that drives it (a hybrid laptop's external display may be on the other GPU), and
//! released once no live view has asked for its frames in IDLE_RELEASE.

use std::collections::HashMap;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::image::Bgra;
use anyhow::{anyhow, bail, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
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

// DXGI and D3D11 objects are free-threaded; this one is only used under the STATE lock.
unsafe impl Send for Duplication {}

enum Live {
    Running(Box<Duplication>),
    /// Duplication failed; GDI until then.
    Failed(Instant),
}

/// One display's duplication, and when a live view last asked for its frames.
struct Output {
    live: Option<Live>,
    last_asked: Instant,
}

struct State {
    /// By GDI device name (`\\.\DISPLAY1`).
    outputs: Option<HashMap<String, Output>>,
    /// The idle reaper runs (one at a time).
    reaper: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    outputs: None,
    reaper: false,
});

/// The newest frame of the display `device` (a GDI device name) at most `max_width` wide, or None
/// when duplication is unavailable for it right now (the caller uses GDI). A new `seq` only when
/// the desktop changed.
pub fn display_frame(device: &str, max_width: u32) -> Option<Frame> {
    let mut state = STATE.lock().unwrap();
    if !state.reaper {
        state.reaper = true;
        thread::spawn(release_when_idle);
    }
    let output = state
        .outputs
        .get_or_insert_with(HashMap::new)
        .entry(device.to_ascii_uppercase())
        .or_insert(Output {
            live: None,
            last_asked: Instant::now(),
        });
    output.last_asked = Instant::now();
    let live = &mut output.live;
    if let Some(Live::Failed(at)) = *live {
        if at.elapsed() < RETRY {
            return None;
        }
        *live = None;
    }
    if live.is_none() {
        *live = Some(match Duplication::open(device) {
            Ok(d) => Live::Running(Box::new(d)),
            Err(error) => {
                eprintln!("[capture] desktop duplication of {device} unavailable, using GDI: {error:#}");
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
            eprintln!("[capture] desktop duplication of {device} stopped: {error:#}");
            *live = Some(Live::Failed(Instant::now()));
            None
        }
    }
}

/// Drops each display's duplication (and its device and textures) once no live view has asked
/// for its frames in IDLE_RELEASE; ends with the last one.
fn release_when_idle() {
    loop {
        thread::sleep(IDLE_RELEASE / 2);
        let mut state = STATE.lock().unwrap();
        let outputs = state.outputs.get_or_insert_with(HashMap::new);
        outputs.retain(|_, output| output.last_asked.elapsed() < IDLE_RELEASE);
        if outputs.is_empty() {
            state.reaper = false;
            return;
        }
    }
}

/// Whether DXGI's output name is this GDI device name.
fn same_device(name: &[u16; 32], device: &str) -> bool {
    let end = name.iter().position(|&u| u == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case(device)
}

impl Duplication {
    /// A duplication of the output whose GDI device name is `device`, on its own adapter. An
    /// empty name (no monitor listed) means the output at the desktop's origin, as before.
    fn open(device: &str) -> Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut found = None;
        let mut adapter_index = 0;
        'adapters: while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
            adapter_index += 1;
            let mut output_index = 0;
            while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
                output_index += 1;
                let desc = unsafe { output.GetDesc()? };
                let origin = desc.DesktopCoordinates;
                let wanted = if device.is_empty() {
                    origin.left == 0 && origin.top == 0
                } else {
                    same_device(&desc.DeviceName, device)
                };
                if desc.AttachedToDesktop.as_bool() && wanted {
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                        && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                    {
                        bail!(
                            "the display is rotated ({:?}); GDI keeps its orientation",
                            desc.Rotation
                        );
                    }
                    found = Some((adapter.cast::<IDXGIAdapter>()?, output));
                    break 'adapters;
                }
            }
        }
        let (adapter, output) =
            found.ok_or_else(|| anyhow!("no GPU output for display {device:?}"))?;
        let mut d3d = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                Some(&mut context),
            )?
        };
        let device = d3d.ok_or_else(|| anyhow!("no D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;
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

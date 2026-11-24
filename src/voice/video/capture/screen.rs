//! Monitor / window capture through Windows Graphics Capture (`windows-capture` 2.0). The
//! capture runs on windows-capture's own thread and converts + encodes in its frame callback;
//! the "rustcord-video-tx" thread supervises it: stop, end of the shared window / monitor, and
//! keyframe requests while the picture is static (Windows only delivers changes).

use super::{Pipeline, Stop};
use crate::voice::video::{MonitorInfo, PixelFormat, RawFrame, ScreenSource, WindowInfo};
use std::ffi::c_void;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
use windows_capture::capture::{CaptureControl, CaptureControlError, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler};
use windows_capture::d3d11::StagingTexture;
use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat, Error as DuplicationError};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, GraphicsCaptureItemType, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// How often the supervisor looks at the capture (its end, idle keyframe requests). A stop
/// does not wait for it.
const TICK: Duration = Duration::from_millis(100);
const GONE: &str = "That screen or window is no longer available.";

pub(super) fn list() -> Result<(Vec<MonitorInfo>, Vec<WindowInfo>), String> {
    let fail = |e: &dyn std::fmt::Display| {
        eprintln!("video: listing screens failed: {e}");
        "Could not list screens and windows.".to_string()
    };
    let primary = Monitor::primary().ok();
    let monitors = Monitor::enumerate()
        .map_err(|e| fail(&e))?
        .into_iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            source: ScreenSource::Monitor(m.as_raw_hmonitor() as u64),
            // The friendly name can be empty (generic / virtual displays).
            name: [m.name(), m.device_string()].into_iter().flatten().find(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("Screen {}", i + 1)),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            primary: Some(m) == primary,
        })
        .collect();
    let own = std::process::id();
    // `enumerate` already skips invisible, tool and child windows (in Z order, front first).
    let windows = Window::enumerate()
        .map_err(|e| fail(&e))?
        .into_iter()
        .filter_map(|w| {
            let title = w.title().ok().filter(|t| !t.trim().is_empty())?;
            let shareable = w.process_id().is_ok_and(|pid| pid != own) && !cloaked(w.as_raw_hwnd());
            shareable.then(|| WindowInfo {
                source: ScreenSource::Window(w.as_raw_hwnd() as u64),
                title,
                process: w.process_name().unwrap_or_default(),
            })
        })
        .collect();
    Ok((monitors, windows))
}

/// Hidden by the shell although "visible" (suspended store apps, other virtual desktops).
fn cloaked(hwnd: *mut c_void) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes one u32 into `cloaked`; a stale HWND only makes it fail.
    let read = unsafe { DwmGetWindowAttribute(HWND(hwnd), DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32) };
    read.is_ok() && cloaked != 0
}

/// Capture until `stop` (Ok) or the capture ends by itself (Err with user-facing text).
pub(super) fn run(source: ScreenSource, pipe: Pipeline, stop: &Stop) -> Result<(), String> {
    let interval = pipe.pacer.interval;
    let control = match source {
        ScreenSource::Monitor(h) => start(Monitor::from_raw_hmonitor(h as usize as *mut c_void), pipe, interval)?,
        ScreenSource::DesktopDuplication(h) => return duplicate(Monitor::from_raw_hmonitor(h as usize as *mut c_void), pipe, stop),
        ScreenSource::Window(h) => {
            let window = Window::from_raw_hwnd(h as usize as *mut c_void);
            if !window.is_valid() {
                return Err(GONE.into());
            }
            start(window, pipe, interval)?
        }
    };
    loop {
        if !matches!(stop.0.recv_timeout(TICK), Err(RecvTimeoutError::Timeout)) {
            let _ = control.stop();
            return Ok(());
        }
        if control.is_finished() {
            return Err(match control.wait() {
                Ok(()) if matches!(source, ScreenSource::Window(_)) => "The shared window was closed.".into(),
                Ok(()) => "The shared screen is no longer available.".into(),
                Err(e) => reason(e),
            });
        }
        let repeated = control.callback().lock().pipe.repeat(Instant::now());
        if let Err(e) = repeated {
            let _ = control.stop();
            return Err(e);
        }
    }
}

fn duplicate(monitor: Monitor, mut pipe: Pipeline, stop: &Stop) -> Result<(), String> {
    let error = |e: DuplicationError| {
        eprintln!("video: desktop duplication: {e}");
        "Desktop Duplication could not read this monitor. Try the default capture mode.".to_owned()
    };
    let open = || -> Result<DxgiDuplicationApi, String> {
        let api = DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8]).map_err(error)?;
        if api.duplication_desc().Rotation != DXGI_MODE_ROTATION_IDENTITY {
            return Err("Use the default capture mode for a rotated monitor.".into());
        }
        Ok(api)
    };
    let mut api = open()?;
    let mut staging: Option<StagingTexture> = None;
    let mut retry = true;
    while matches!(stop.0.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
        let lost = match api.acquire_next_frame(30) {
            Ok(mut frame) => {
                retry = true;
                let now = Instant::now();
                if frame.frame_info().LastPresentTime == 0 || !pipe.due(now)? {
                    pipe.repeat(now)?;
                    continue;
                }
                let (width, height, format) = (frame.width(), frame.height(), frame.texture_desc().Format);
                let texture = match &mut staging {
                    Some(texture) if (texture.desc().Width, texture.desc().Height, texture.desc().Format) == (width, height, format) => texture,
                    slot => slot.insert(StagingTexture::new(frame.device(), width, height, format).map_err(|e| read_failed(&e))?),
                };
                let mut buffer = frame.buffer_with(texture).map_err(error)?;
                let stride = buffer.row_pitch() as usize;
                pipe.push(&RawFrame { width, height, stride, format: PixelFormat::Bgra, data: buffer.as_raw_buffer() }, now)?;
                false
            }
            Err(DuplicationError::Timeout) => {
                pipe.repeat(Instant::now())?;
                false
            }
            Err(DuplicationError::AccessLost) if retry => true,
            Err(e) => return Err(error(e)),
        };
        if lost {
            retry = false;
            staging = None;
            drop(api);
            api = open()?;
        }
    }
    Ok(())
}

/// Start Windows Graphics Capture of `item` on its own thread: BGRA, cursor on, no yellow
/// border, at most `interval` between updates where the OS supports each setting.
fn start<T>(item: T, pipe: Pipeline, interval: Duration) -> Result<CaptureControl<Capture, String>, String>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let supported = |check: fn() -> Result<bool, windows_capture::graphics_capture_api::Error>| check().unwrap_or(false);
    let settings = Settings::new(
        item,
        if supported(GraphicsCaptureApi::is_cursor_settings_supported) { CursorCaptureSettings::WithCursor } else { CursorCaptureSettings::Default },
        if supported(GraphicsCaptureApi::is_border_settings_supported) { DrawBorderSettings::WithoutBorder } else { DrawBorderSettings::Default },
        SecondaryWindowSettings::Default,
        // shortcut: without this OS throttle (before Windows 11 24H2) the pacer drops frames, and a
        // change that lands only in a dropped frame shows once the screen changes again; keep the
        // last frame on the GPU if that matters.
        if supported(GraphicsCaptureApi::is_minimum_update_interval_supported) {
            MinimumUpdateIntervalSettings::Custom(interval)
        } else {
            MinimumUpdateIntervalSettings::Default
        },
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        pipe,
    );
    Capture::start_free_threaded(settings).map_err(|e| {
        eprintln!("video: screen capture did not start: {e}");
        match e {
            GraphicsCaptureApiError::ItemConvertFailed => GONE.into(),
            _ => "Could not start the screen share.".into(),
        }
    })
}

/// User-facing text for a capture thread that ended with an error.
fn reason(e: CaptureControlError<String>) -> String {
    match e {
        CaptureControlError::GraphicsCaptureApiError(GraphicsCaptureApiError::FrameHandlerError(reason)) => reason,
        e => {
            eprintln!("video: screen capture ended: {e}");
            "The screen share stopped unexpectedly.".into()
        }
    }
}

/// windows-capture's frame handler: the pipeline, fed on its capture thread.
struct Capture {
    pipe: Pipeline,
    /// The CPU-readable copy of a frame, reused while size and format stay (`Frame::buffer`
    /// would create one per frame).
    staging: Option<StagingTexture>,
}

fn read_failed(e: &dyn std::fmt::Display) -> String {
    eprintln!("video: reading a screen frame failed: {e}");
    "The screen share stopped unexpectedly.".into()
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = Pipeline;
    type Error = String;

    fn new(ctx: Context<Pipeline>) -> Result<Capture, String> {
        Ok(Capture { pipe: ctx.flags, staging: None })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _: InternalCaptureControl) -> Result<(), String> {
        let now = Instant::now();
        // Before the copy: the GPU → CPU copy is the expensive part.
        if !self.pipe.due(now)? {
            return Ok(());
        }
        let (width, height, format) = (frame.desc().Width, frame.desc().Height, frame.desc().Format);
        let staging = match &mut self.staging {
            Some(s) if (s.desc().Width, s.desc().Height, s.desc().Format) == (width, height, format) => s,
            slot => slot.insert(StagingTexture::new(frame.device(), width, height, format).map_err(|e| read_failed(&e))?),
        };
        let context = frame.device_context();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture matches the frame's size and format; it stays mapped only
        // while `data` is used (D3D11 maps `RowPitch × height` bytes of a 2D texture).
        unsafe {
            context.CopyResource(staging.texture(), frame.as_raw_texture());
            context.Map(staging.texture(), 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| read_failed(&e))?;
            let stride = mapped.RowPitch as usize;
            let data = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), stride * height as usize);
            let result = self.pipe.push(&RawFrame { width, height, stride, format: PixelFormat::Bgra, data }, now);
            context.Unmap(staging.texture(), 0);
            result
        }
    }
}

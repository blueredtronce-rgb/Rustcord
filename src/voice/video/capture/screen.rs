//! Monitor / window capture through Windows Graphics Capture (`windows-capture` 2.0). The
//! capture runs on windows-capture's own thread and converts + encodes in its frame callback;
//! the "rustcord-video-tx" thread supervises it: stop, end of the shared window / monitor, and
//! keyframe requests while the picture is static (Windows only delivers changes).

mod cursor;
mod gpu;
mod nvenc;
mod scale;

use super::{Pipeline, Stop};
use crate::voice::video::trace::Metric;
use crate::voice::video::{MonitorInfo, PixelFormat, RawFrame, ScreenSource, WindowInfo};
use std::ffi::c_void;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{D3D11_MAP_READ, D3D11_MAP_READ_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_MODE_ROTATION_IDENTITY};
use windows::core::Interface;
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
    #[cfg(feature = "media-diagnostics")]
    let pipe = {
        let mut pipe = pipe;
        pipe.screen_method = match source {
            ScreenSource::Monitor(_) => "Windows Graphics Capture / monitor",
            ScreenSource::DesktopDuplication(_) => "Desktop Duplication / monitor",
            ScreenSource::Window(_) => "Windows Graphics Capture / window",
        };
        encoder_details(&mut pipe, "OpenH264 CPU");
        pipe
    };
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
        let repeated = {
            let callback = control.callback();
            let mut capture = callback.lock();
            let Capture { pipe, readback } = &mut *capture;
            readback.repeat(pipe, Instant::now())
        };
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
    let mut readback = Readback::default();
    let mut cursor = cursor::Cursor::default();
    let mut next_capture = Instant::now();
    let mut retry = true;
    while matches!(stop.0.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
        let delay = next_capture.saturating_duration_since(Instant::now());
        if !delay.is_zero() && !matches!(stop.0.recv_timeout(delay), Err(RecvTimeoutError::Timeout)) {
            return Ok(());
        }
        let lost = match api.acquire_next_frame(30) {
            Ok(frame) => {
                retry = true;
                let now = Instant::now();
                if let Some(trace) = &mut pipe.trace { trace.hit(Metric::CaptureArrived); }
                cursor.update(&frame);
                let info = frame.frame_info();
                let unchanged = info.LastPresentTime == 0 && info.LastMouseUpdateTime == 0 && pipe.last_encoded.is_some();
                if unchanged && let Some(trace) = &mut pipe.trace { trace.hit(Metric::CaptureUnchanged); }
                if unchanged || !pipe.due(now)? {
                    drop(frame);
                    next_capture = now + Duration::from_millis(4);
                    readback.repeat(&mut pipe, now)?;
                    continue;
                }
                next_capture = pipe.pacer.next.unwrap_or(now);
                readback.prepare(&mut pipe, frame.device(), frame.device_context(), frame.texture(), Some(&cursor), now)?;
                drop(frame);
                readback.submit(&mut pipe, now)?;
                false
            }
            Err(DuplicationError::Timeout) => {
                if let Some(trace) = &mut pipe.trace { trace.hit(Metric::CaptureTimeout); }
                readback.repeat(&mut pipe, Instant::now())?;
                false
            }
            Err(DuplicationError::AccessLost) if retry => true,
            Err(e) => return Err(error(e)),
        };
        if lost {
            retry = false;
            readback = Readback::default();
            cursor = cursor::Cursor::default();
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
    readback: Readback,
}

fn read_failed(e: &dyn std::fmt::Display) -> String {
    eprintln!("video: reading a screen frame failed: {e}");
    "The screen share stopped unexpectedly.".into()
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = Pipeline;
    type Error = String;

    fn new(ctx: Context<Pipeline>) -> Result<Capture, String> {
        Ok(Capture { pipe: ctx.flags, readback: Readback::default() })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _: InternalCaptureControl) -> Result<(), String> {
        let now = Instant::now();
        if let Some(trace) = &mut self.pipe.trace { trace.hit(Metric::CaptureArrived); }
        // Before the copy: the GPU → CPU copy is the expensive part.
        if !self.pipe.due(now)? {
            return Ok(());
        }
        self.readback.prepare(&mut self.pipe, frame.device(), frame.device_context(), frame.as_raw_texture(), None, now)?;
        self.readback.submit(&mut self.pipe, now)
    }
}

#[derive(Default)]
struct Readback {
    hardware_source: Option<(usize, u32, u32, DXGI_FORMAT)>,
    hardware_retry: Option<Instant>,
    hardware: Option<gpu::Hardware>,
    source: Option<(u32, u32, DXGI_FORMAT)>,
    scale: Option<scale::Scale>,
    staging: Option<StagingTexture>,
}

impl Readback {
    fn prepare(&mut self, pipe: &mut Pipeline, device: &ID3D11Device, context: &ID3D11DeviceContext, texture: &ID3D11Texture2D, cursor: Option<&cursor::Cursor>, now: Instant) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let size = (desc.Width, desc.Height);
        let source = (device.as_raw() as usize, size.0, size.1, desc.Format);
        if self.hardware_source != Some(source) {
            let first = self.hardware_source.is_none();
            self.hardware_source = Some(source);
            // While resizing a window, use the existing software path until the new source settles.
            self.hardware_retry = Some(if first { now } else { now + super::RESIZE_SETTLE });
            if self.hardware.take().is_some() {
                pipe.keyframe.store(true, Ordering::Release);
                encoder_details(pipe, "OpenH264 CPU");
            }
        }
        if self.hardware_retry.is_some_and(|retry| now >= retry) {
            self.hardware_retry = None;
            match gpu::Hardware::new(device, context, size, desc.Format, pipe.limits) {
                Ok(hardware) => {
                    encoder_details(pipe, hardware.label());
                    self.hardware = Some(hardware);
                    self.scale = None;
                    self.staging = None;
                    self.source = None;
                    pipe.enc = None;
                    pipe.pending_size = None;
                    pipe.keyframe.store(true, Ordering::Release);
                }
                Err(error) => {
                    encoder_details(pipe, "OpenH264 CPU");
                    eprintln!("video: hardware screen encoder unavailable: {error}");
                }
            }
        }
        if let Some(hardware) = &mut self.hardware {
            match hardware.prepare(texture, cursor, pipe) {
                Ok(()) => return Ok(()),
                Err(error) => self.disable_hardware(pipe, &error),
            }
        }
        self.prepare_software(pipe, device, context, texture, cursor, now, desc)
    }

    fn submit(&mut self, pipe: &mut Pipeline, now: Instant) -> Result<(), String> {
        if self.hardware.is_none() { return pipe.submit(now); }
        self.encode_hardware(pipe, now)?;
        if let Some(hardware) = &mut self.hardware { hardware.preview(pipe, now, false); }
        Ok(())
    }

    fn repeat(&mut self, pipe: &mut Pipeline, now: Instant) -> Result<(), String> {
        let Some(hardware) = &mut self.hardware else { return pipe.repeat(now); };
        if pipe.closed() { return Err(super::CLOSED.into()); }
        hardware.preview(pipe, now, true);
        if let Some(trace) = &mut pipe.trace { trace.flush(now); }
        if pipe.last_encoded.is_some_and(|time| now >= time + pipe.pacer.interval)
            && pipe.keyframe.load(Ordering::Acquire) && pipe.tx.capacity() > 0 {
            self.encode_hardware(pipe, now)?;
        }
        Ok(())
    }

    fn encode_hardware(&mut self, pipe: &mut Pipeline, now: Instant) -> Result<(), String> {
        let Some(hardware) = &mut self.hardware else { return Ok(()); };
        let keyframe = pipe.keyframe.swap(false, Ordering::AcqRel);
        pipe.last_encoded = Some(now);
        let started = pipe.trace.as_ref().map(|_| Instant::now());
        let encoded = hardware.encode(keyframe, now);
        let driver_failed = encoded.is_err();
        match pipe.send_encoded(encoded, keyframe, started) {
            Err(error) if driver_failed && !pipe.closed() => {
                self.disable_hardware(pipe, &error);
                Ok(())
            }
            result => result,
        }
    }

    fn disable_hardware(&mut self, pipe: &mut Pipeline, error: &str) {
        eprintln!("video: switching screen encoder to OpenH264: {error}");
        self.hardware = None;
        pipe.keyframe.store(true, Ordering::Release);
        encoder_details(pipe, "OpenH264 CPU fallback");
    }

    fn prepare_software(&mut self, pipe: &mut Pipeline, device: &ID3D11Device, context: &ID3D11DeviceContext, texture: &ID3D11Texture2D, cursor: Option<&cursor::Cursor>, now: Instant, desc: D3D11_TEXTURE2D_DESC) -> Result<(), String> {
        let at = pipe.trace.as_ref().map(|_| Instant::now());
        let input = (desc.Width, desc.Height, desc.Format);
        let original = (desc.Width, desc.Height);
        let scaled = super::codec::fit_within(desc.Width, desc.Height, pipe.limits.max_width, pipe.limits.max_height);
        if self.source != Some(input) {
            self.source = Some(input);
            self.scale = (scaled.0 <= original.0 && scaled.1 <= original.1 && scaled != original)
                .then(|| scale::Scale::new(device, context, original, scaled, desc.Format, pipe.limits.fps).ok()).flatten();
        }
        if self.scale.as_ref().is_some_and(|scaler| scaler.frame(texture).is_err()) {
            self.scale = None;
        }
        let ((width, height), texture) = self.scale.as_ref().map_or((original, texture), |scaler| (scaled, scaler.output_texture()));
        let format = desc.Format;
        let staging = match &mut self.staging {
            Some(s) if (s.desc().Width, s.desc().Height, s.desc().Format) == (width, height, format) => s,
            slot => slot.insert(StagingTexture::new(device, width, height, format).map_err(|e| read_failed(&e))?),
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let mode = if cursor.is_some() { D3D11_MAP_READ_WRITE } else { D3D11_MAP_READ };
        // The staging texture has the selected texture's size and format. Its slice ends at Unmap.
        unsafe {
            context.CopyResource(staging.texture(), texture);
            context.Map(staging.texture(), 0, mode, 0, Some(&mut mapped)).map_err(|e| read_failed(&e))?;
            if let (Some(trace), Some(at)) = (&mut pipe.trace, at) {
                trace.record(if self.scale.is_some() { Metric::GpuScaleReadback } else { Metric::CpuReadback }, at.elapsed());
            }
            let stride = mapped.RowPitch as usize;
            if let Some(cursor) = cursor {
                let data = std::slice::from_raw_parts_mut(mapped.pData.cast::<u8>(), stride * height as usize);
                cursor.paint(original, (width, height), stride, data);
            }
            let data = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), stride * height as usize);
            let result = pipe.prepare(&RawFrame { width, height, stride, format: PixelFormat::Bgra, data }, now);
            context.Unmap(staging.texture(), 0);
            result
        }
    }
}

fn encoder_details(_pipe: &mut Pipeline, _encoder: &str) {
    #[cfg(feature = "media-diagnostics")]
    if let Some(trace) = &mut _pipe.trace {
        let limits = _pipe.limits;
        trace.details(format!("{}; target {}x{} at {} FPS, {} kbit/s; {}",
            _pipe.screen_method, limits.max_width, limits.max_height, limits.fps, limits.max_bitrate / 1000, _encoder));
    }
}

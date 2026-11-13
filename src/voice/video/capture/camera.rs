//! Camera capture through Media Foundation: the device list (`MFEnumDeviceSources`), and an
//! `IMFSourceReader` in synchronous mode on the capture thread. The reader runs the camera in
//! its best native mode within the send limits (`pick_mode`) and hands out NV12 (its video
//! processor decodes / converts MJPG and YUY2). Releasing the reader shuts the device down.

use super::{Pipeline, Stop};
use crate::voice::video::{CameraInfo, PixelFormat, RawFrame, SendLimits};
use std::time::Instant;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_VERSION, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFSTARTUP_NOSOCKET, MFShutdown, MFStartup, MFVideoFormat_NV12, MFVideoFormat_YUY2,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::core::{GUID, PWSTR};

const NO_CAMERA: &str = "No camera found.";
const GONE: &str = "That camera is no longer connected.";
const LOST: &str = "The camera stopped.";
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Native formats looked at (cameras list a few dozen).
const MAX_MODES: u32 = 512;

/// COM (MTA) + Media Foundation on the current thread, shut down on drop.
struct Mf {
    _com: Com,
}

struct Com(bool);

impl Mf {
    fn start() -> Result<Mf, String> {
        // SAFETY: per-thread initialization, balanced by the drops below (only when it succeeded).
        let com = Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok());
        // SAFETY: as above; MFShutdown in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| {
            eprintln!("video: MFStartup failed: {e}");
            "Camera capture is not available on this system.".to_string()
        })?;
        Ok(Mf { _com: com })
    }
}

impl Drop for Mf {
    fn drop(&mut self) {
        // SAFETY: balances the MFStartup in `start`; every MF object of this thread is gone.
        let _ = unsafe { MFShutdown() };
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances a successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() }
        }
    }
}

pub(super) fn list() -> Result<Vec<CameraInfo>, String> {
    let _mf = Mf::start()?;
    let devices = devices()?;
    Ok(devices
        .iter()
        .filter_map(|d| {
            let id = string(d, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)?;
            let name = string(d, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_else(|| "Camera".into());
            Some(CameraInfo { id, name })
        })
        .collect())
}

/// Capture until `stop` (Ok) or the camera fails (Err with user-facing text).
pub(super) fn run(device: Option<&str>, mut pipe: Pipeline, stop: &Stop) -> Result<(), String> {
    let _mf = Mf::start()?;
    let reader = open(device, pipe.limits)?;
    let mut format = current_format(&reader)?;
    loop {
        let (mut flags, mut sample) = (0u32, None);
        // SAFETY: a synchronous read into locals that outlive the call.
        unsafe { reader.ReadSample(STREAM, 0, None, Some(&raw mut flags), None, Some(&raw mut sample)) }.map_err(|e| {
            eprintln!("video: camera read failed: {e}");
            LOST.to_string()
        })?;
        // After the (blocking) read: a frame that arrives after Stop is never used.
        if stop.requested() {
            return Ok(());
        }
        if flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32 != 0 {
            return Err(LOST.into());
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            format = current_format(&reader)?;
        }
        let Some(sample) = sample else { continue };
        let now = Instant::now();
        if pipe.due(now)? {
            push(&sample, format, &mut pipe, now)?;
        }
    }
}

/// Size and row stride of the reader's NV12 output.
#[derive(Debug, Clone, Copy)]
struct Format {
    width: u32,
    height: u32,
    /// `MF_MT_DEFAULT_STRIDE` when set and positive.
    stride: Option<u32>,
}

fn push(sample: &IMFSample, f: Format, pipe: &mut Pipeline, now: Instant) -> Result<(), String> {
    // SAFETY: the buffer is locked while `data` is used and unlocked right after.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(|_| LOST)?;
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut ptr, None, Some(&raw mut len)).map_err(|_| LOST)?;
        let result = if ptr.is_null() {
            Ok(())
        } else {
            let data = std::slice::from_raw_parts(ptr, len as usize);
            match nv12_stride(data.len(), f.width, f.height, f.stride) {
                Some(stride) => pipe.push(&RawFrame { width: f.width, height: f.height, stride, format: PixelFormat::Nv12, data }, now),
                None => Ok(()), // a short buffer: skip the frame
            }
        };
        let _ = buffer.Unlock();
        result
    }
}

/// The video-capture devices; each one owns its reference.
fn devices() -> Result<Vec<IMFActivate>, String> {
    let fail = |e: windows::core::Error| {
        eprintln!("video: camera enumeration failed: {e}");
        "Could not list cameras.".to_string()
    };
    // SAFETY: MF calls with valid out-pointers; the returned array is freed once its entries
    // were moved out.
    unsafe {
        let attrs = attributes().map_err(fail)?;
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(fail)?;
        let (mut list, mut count) = (std::ptr::null_mut(), 0u32);
        MFEnumDeviceSources(&attrs, &mut list, &mut count).map_err(fail)?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        let out = std::slice::from_raw_parts_mut(list, count as usize).iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(list as *const _));
        Ok(out)
    }
}

fn attributes() -> windows::core::Result<IMFAttributes> {
    let mut attrs = None;
    // SAFETY: valid out-pointer.
    unsafe { MFCreateAttributes(&mut attrs, 1)? };
    attrs.ok_or_else(windows::core::Error::empty)
}

fn string(device: &IMFActivate, key: &GUID) -> Option<String> {
    let (mut s, mut len) = (PWSTR::null(), 0u32);
    // SAFETY: on success `s` is a CoTaskMem string we free after copying it.
    unsafe {
        device.GetAllocatedString(key, &mut s, &mut len).ok()?;
        let out = s.to_string().ok();
        CoTaskMemFree(Some(s.0 as *const _));
        out
    }
}

/// The camera `device` (`CameraInfo::id`, None = the first), in its best mode, NV12 out.
fn open(device: Option<&str>, limits: SendLimits) -> Result<IMFSourceReader, String> {
    let devices = devices()?;
    let activate = match device {
        None => devices.first().ok_or(NO_CAMERA)?,
        Some(id) => devices.iter().find(|d| string(d, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).as_deref() == Some(id)).ok_or(GONE)?,
    };
    // SAFETY: MF calls on live objects; a source whose reader could not be made is shut down.
    unsafe {
        let source: IMFMediaSource = activate.ActivateObject().map_err(open_error)?;
        let reader = attributes()
            .and_then(|attrs| {
                attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
                MFCreateSourceReaderFromMediaSource(&source, &attrs)
            })
            .map_err(|e| {
                let _ = source.Shutdown();
                open_error(e)
            })?;
        let (types, modes): (Vec<IMFMediaType>, Vec<Mode>) =
            (0..MAX_MODES).map_while(|i| reader.GetNativeMediaType(STREAM, i).ok()).filter_map(|t| mode(&t).map(|m| (t, m))).unzip();
        let native = pick_mode(&modes, limits).map(|i| &types[i]);
        if let Some(native) = native {
            reader.SetCurrentMediaType(STREAM, None, native).map_err(open_error)?;
        }
        let nv12 = MFCreateMediaType().and_then(|t| {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            if let Some(native) = native {
                // The camera's own size and rate: the reader only converts the pixels.
                t.SetUINT64(&MF_MT_FRAME_SIZE, native.GetUINT64(&MF_MT_FRAME_SIZE)?)?;
                t.SetUINT64(&MF_MT_FRAME_RATE, native.GetUINT64(&MF_MT_FRAME_RATE)?)?;
            }
            reader.SetCurrentMediaType(STREAM, None, &t)
        });
        nv12.map_err(|e| {
            eprintln!("video: camera NV12 output refused: {e}");
            "This camera's video format is not supported.".to_string()
        })?;
        Ok(reader)
    }
}

fn current_format(reader: &IMFSourceReader) -> Result<Format, String> {
    // SAFETY: attribute reads on a live media type.
    unsafe {
        let t = reader.GetCurrentMediaType(STREAM).map_err(|_| LOST)?;
        if t.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
            return Err("This camera's video format is not supported.".into());
        }
        let (width, height) = split(t.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|_| LOST)?);
        let stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok().filter(|&s| (s as i32) > 0);
        Ok(Format { width, height, stride })
    }
}

fn open_error(e: windows::core::Error) -> String {
    eprintln!("video: camera open failed: {e}");
    if e.code() == E_ACCESSDENIED {
        "Windows blocked camera access. Allow it under Settings > Privacy & security > Camera.".into()
    } else {
        "Could not open the camera. It may be in use by another app.".into()
    }
}

/// A packed MF attribute pair: (high 32 bits, low 32 bits), e.g. (width, height).
fn split(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// One native camera format.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mode {
    width: u32,
    height: u32,
    /// Frames per 1000 s.
    millifps: u32,
    /// 2 = NV12, 1 = YUY2 (no conversion / cheap), 0 = compressed (MJPG …, decoded on the CPU).
    rank: u8,
}

fn mode(t: &IMFMediaType) -> Option<Mode> {
    // SAFETY: attribute reads on a live media type.
    unsafe {
        let (width, height) = split(t.GetUINT64(&MF_MT_FRAME_SIZE).ok()?);
        let (num, den) = split(t.GetUINT64(&MF_MT_FRAME_RATE).ok()?);
        let subtype = t.GetGUID(&MF_MT_SUBTYPE).ok()?;
        let millifps = if den == 0 { 0 } else { (u64::from(num) * 1000 / u64::from(den)).min(u64::from(u32::MAX)) as u32 };
        let rank = if subtype == MFVideoFormat_NV12 { 2 } else { u8::from(subtype == MFVideoFormat_YUY2) };
        Some(Mode { width, height, millifps, rank })
    }
}

/// The native mode to run the camera in: within the send box if any is, at the send frame rate
/// if any reaches it (29.97 counts as 30), then the largest picture (the smallest above the box
/// when none fits), uncompressed before MJPG, and the lowest frame rate that still qualifies.
fn pick_mode(modes: &[Mode], limits: SendLimits) -> Option<usize> {
    let key = |m: &Mode| {
        let fits = m.width <= limits.max_width && m.height <= limits.max_height;
        let area = i64::from(m.width) * i64::from(m.height);
        (fits, m.millifps.saturating_add(500) >= limits.fps * 1000, if fits { area } else { -area }, m.rank, std::cmp::Reverse(m.millifps))
    };
    (0..modes.len()).max_by_key(|&i| key(&modes[i]))
}

/// Row stride of an NV12 buffer of `len` bytes: the declared one when the buffer holds it,
/// else the width; None when the buffer is too short for either.
fn nv12_stride(len: usize, width: u32, height: u32, declared: Option<u32>) -> Option<usize> {
    let (w, h) = (width as usize, height as usize);
    let rows = h + h.div_ceil(2);
    [declared.map(|s| s as usize), Some(w)].into_iter().flatten().find(|&s| s >= w && w > 0 && s * rows <= len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: u32, height: u32, fps: f64, rank: u8) -> Mode {
        Mode { width, height, millifps: (fps * 1000.0) as u32, rank }
    }

    #[test]
    fn picks_the_largest_mode_at_full_rate_within_the_box() {
        let cam = SendLimits::CAMERA;
        let typical = [m(640, 480, 30.0, 1), m(1280, 720, 10.0, 1), m(1280, 720, 30.0, 0), m(1920, 1080, 30.0, 0)];
        assert_eq!(pick_mode(&typical, cam), Some(2), "720p30 MJPG over 720p10 YUY2 and 1080p");
        assert_eq!(pick_mode(&[m(1280, 720, 5.0, 1), m(640, 360, 30.0, 2)], cam), Some(1), "frame rate before size");
        assert_eq!(pick_mode(&[m(1280, 720, 29.97, 0), m(960, 540, 30.0, 0)], cam), Some(0), "29.97 counts as 30");
        assert_eq!(pick_mode(&[m(1280, 720, 60.0, 0), m(1280, 720, 30.0, 0)], cam), Some(1), "no faster than needed");
        assert_eq!(pick_mode(&[m(1280, 720, 30.0, 0), m(1280, 720, 30.0, 2), m(1280, 720, 30.0, 1)], cam), Some(1), "NV12 first");
        assert_eq!(pick_mode(&[m(3840, 2160, 30.0, 0), m(1920, 1080, 30.0, 0)], cam), Some(1), "none fits: the smallest above");
        assert_eq!(pick_mode(&[m(640, 480, 15.0, 1), m(320, 240, 15.0, 1)], cam), Some(0), "none reaches 30: the largest");
        assert_eq!(pick_mode(&[], cam), None);
    }

    #[test]
    fn nv12_strides() {
        assert_eq!(nv12_stride(1280 * 720 * 3 / 2, 1280, 720, None), Some(1280));
        assert_eq!(nv12_stride(1280 * 720 * 3 / 2, 1280, 720, Some(1280)), Some(1280));
        assert_eq!(nv12_stride(1344 * 1080, 1280, 720, Some(1344)), Some(1344), "padded rows");
        assert_eq!(nv12_stride(1280 * 1080, 1280, 720, Some(1344)), Some(1280), "declared stride does not fit the buffer");
        assert_eq!(nv12_stride(1280 * 1080 - 1, 1280, 720, None), None, "short buffer");
        assert_eq!(nv12_stride(6, 2, 2, Some(1)), Some(2), "a stride below the width is ignored");
        assert_eq!(nv12_stride(100, 0, 0, None), None);
    }

    #[test]
    fn packed_attribute_pairs() {
        assert_eq!(split((1280 << 32) | 720), (1280, 720));
        assert_eq!(split((30_000 << 32) | 1001), (30_000, 1001));
    }
}

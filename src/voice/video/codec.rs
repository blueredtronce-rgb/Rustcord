//! H.264 encode/decode with openh264 0.9 (Constrained Baseline, Annex-B) and the pixel work
//! around it: captured BGRA/NV12 → I420 with scaling, I420 → RGBA (decoded pictures downscaled
//! into `MAX_FRAME_SIZE`, my own preview). Pure CPU work, no I/O, no threads (callers run it on
//! their own thread). Colours are BT.601 limited range both ways, like openh264 and libyuv.
//!
//! OWNERSHIP: codec engineer. Keep the pub signatures (capture.rs and receive.rs use them).
//! Spec: VOICE_NOTES §5.1/§5.3, docs/VIDEO_DESIGN.md "Codec".

use super::{EncodedFrame, FrameWriter, MAX_FRAME_SIZE, PixelFormat, RawFrame, SendLimits};
use openh264::encoder::{BitRate, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode, UsageType};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};
use std::time::Instant;

/// Seconds between periodic IDRs (a receiver whose keyframe request got lost recovers by then).
const KEYFRAME_SECS: u32 = 5;
/// openh264's encoder limit.
const MAX_ENCODE_SIZE: (u32, u32) = (3840, 2160);

/// One I420 picture (even size): the send pipeline's reusable scratch, written by `to_i420`
/// and read by `Encoder::encode`.
pub struct I420 {
    width: usize,
    height: usize,
    /// Y plane, then U, then V (each chroma plane width/2 × height/2).
    data: Vec<u8>,
}

impl I420 {
    /// Panics on an odd or empty size (`fit_within` sizes are even and at least 2×2).
    pub fn new(width: u32, height: u32) -> I420 {
        assert!(width >= 2 && height >= 2 && width.is_multiple_of(2) && height.is_multiple_of(2), "I420 needs an even size, got {width}×{height}");
        let (width, height) = (width as usize, height as usize);
        I420 { width, height, data: vec![0; width * height * 3 / 2] }
    }

    fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let (y, uv) = self.data.split_at_mut(self.width * self.height);
        let (u, v) = uv.split_at_mut(uv.len() / 2);
        (y, u, v)
    }
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    fn u(&self) -> &[u8] {
        let y = self.width * self.height;
        &self.data[y..y + y / 4]
    }

    fn v(&self) -> &[u8] {
        let y = self.width * self.height;
        &self.data[y + y / 4..]
    }
}

/// openh264 encoder of one send pipeline.
pub struct Encoder {
    inner: openh264::encoder::Encoder,
    /// Capture time of the first frame: openh264's rate control wants millisecond timestamps.
    start: Option<Instant>,
}

impl Encoder {
    /// Encoder for `width × height` (even, from `fit_within`) at `limits.fps` and at most
    /// `limits.max_bitrate`: Constrained Baseline (every receiver decodes it), no B-frames,
    /// SPS/PPS before every IDR, an IDR every few seconds and on `keyframe`. Screen content
    /// uses the screen usage type.
    pub fn new(width: u32, height: u32, limits: SendLimits, screen: bool) -> Result<Encoder, String> {
        let (max_w, max_h) = MAX_ENCODE_SIZE;
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) || width.max(height) > max_w || width.min(height) > max_h {
            return Err(format!("Cannot encode video of {width}×{height}."));
        }
        let fps = limits.fps.clamp(1, 60);
        // Baseline + openh264's constraint_set0/1 flags = Constrained Baseline (profile-level-id 42c0..).
        let config = EncoderConfig::new()
            .profile(Profile::Baseline)
            .usage_type(if screen { UsageType::ScreenContentRealTime } else { UsageType::CameraVideoRealTime })
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(limits.max_bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * KEYFRAME_SECS))
            .skip_frames(true);
        let inner = openh264::encoder::Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| format!("Could not start the video encoder: {e}"))?;
        Ok(Encoder { inner, start: None })
    }

    /// Encode one picture (normally of the encoder's size; another size re-initializes the
    /// encoder and starts with an IDR). `keyframe` forces an IDR. Ok(None) = the rate control
    /// skipped the frame (a forced IDR then comes with the next one).
    pub fn encode(&mut self, picture: &impl YUVSource, keyframe: bool, captured: Instant) -> Result<Option<EncodedFrame>, String> {
        let start = *self.start.get_or_insert(captured);
        if keyframe {
            // Before the first frame this is a no-op (openh264 checks); the first frame is an IDR anyway.
            self.inner.force_intra_frame();
        }
        let ms = u64::try_from(captured.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX);
        let bits = self.inner.encode_at(picture, Timestamp::from_millis(ms)).map_err(|e| format!("Video encoding failed: {e}"))?;
        let kind = bits.frame_type();
        if matches!(kind, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let annexb = bits.to_vec();
        Ok((!annexb.is_empty()).then_some(EncodedFrame { annexb, keyframe: kind == FrameType::IDR, captured }))
    }
}

/// openh264 decoder of one received video.
pub struct Decoder {
    inner: openh264::decoder::Decoder,
}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        let inner = openh264::decoder::Decoder::new().map_err(|e| format!("Could not start the video decoder: {e}"))?;
        Ok(Decoder { inner })
    }

    /// Decode one access unit (Annex-B, 4-byte start codes, already E2EE-decrypted) and write
    /// the picture as RGBA, downscaled into `MAX_FRAME_SIZE`, to `out`. Ok(Some(size)) = a
    /// picture of that size was written; Ok(None) = none yet; Err = broken stream (the caller
    /// sends a PLI and waits for the next keyframe).
    pub fn decode(&mut self, annexb: &[u8], out: &FrameWriter) -> Result<Option<(u32, u32)>, String> {
        let Some(yuv) = self.inner.decode(annexb).map_err(|e| format!("H.264 decoding failed: {e}"))? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let (w, h) = (u32::try_from(w).unwrap_or(0), u32::try_from(h).unwrap_or(0));
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let (max_w, max_h) = MAX_FRAME_SIZE;
        let written = if w <= max_w && h <= max_h {
            // openh264's SIMD conversion; its size check matches `write`'s buffer.
            out.write(w, h, |rgba| yuv.write_rgba8(rgba)).then_some((w, h))
        } else {
            let (fw, fh) = fit_within(w, h, max_w, max_h);
            out.write(fw, fh, |rgba| i420_to_rgba(&yuv, (fw, fh), rgba)).then_some((fw, fh))
        };
        Ok(written)
    }
}

/// The largest even size within `max_w × max_h` with the aspect ratio of `w × h`; never
/// upscales, never below 2×2. (1920×1080 into 1280×720 = 1280×720; 1000×1000 into 1280×720 =
/// 720×720.)
pub fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (w64, h64) = (u64::from(w.max(1)), u64::from(h.max(1)));
    let (bw, bh) = (u64::from(max_w).min(w64), u64::from(max_h).min(h64));
    // Width-limited when bw/w ≤ bh/h.
    let (fw, fh) = if bw * h64 <= bh * w64 { (bw, bw * h64 / w64) } else { (bh * w64 / h64, bh) };
    let even = |v: u64| (u32::try_from(v).unwrap_or(u32::MAX) & !1).max(2);
    (even(fw), even(fh))
}

/// Convert (and scale) a captured frame into `dst`, whose size is the target size; the picture
/// is stretched to fill it (keep the aspect with `fit_within`). A frame whose `data` is too
/// short for its size and stride is ignored (`dst` keeps the previous picture).
pub fn to_i420(src: &RawFrame<'_>, dst: &mut I420) {
    let (sw, sh, stride) = (src.width as usize, src.height as usize, src.stride);
    let (dw, dh) = (dst.width, dst.height);
    let (cw, ch) = (dw / 2, dh / 2);
    let (y_out, u_out, v_out) = dst.planes_mut();
    let data = src.data;
    match src.format {
        PixelFormat::Bgra => {
            if sw == 0 || sh == 0 || stride < sw * 4 || data.len() < stride * (sh - 1) + sw * 4 {
                return;
            }
            let rgb = |x: usize, y: usize| {
                let p = &data[y * stride + x * 4..][..3];
                [u32::from(p[2]), u32::from(p[1]), u32::from(p[0])]
            };
            resample((sw, sh), (dw, dh), |x, y| [luma(rgb(x, y))], |x, y, [l]| y_out[y * dw + x] = l as u8);
            // Chroma of the averaged colour = average of the chroma (the matrix is linear).
            resample((sw, sh), (cw, ch), rgb, |x, y, c| {
                let (u, v) = chroma(c);
                u_out[y * cw + x] = u;
                v_out[y * cw + x] = v;
            });
        }
        PixelFormat::Nv12 => {
            let (scw, sch) = (sw.div_ceil(2), sh.div_ceil(2));
            if sw == 0 || sh == 0 || stride < scw * 2 || data.len() < stride * (sh + sch - 1) + scw * 2 {
                return;
            }
            let uv = &data[stride * sh..];
            resample((sw, sh), (dw, dh), |x, y| [u32::from(data[y * stride + x])], |x, y, [l]| y_out[y * dw + x] = l as u8);
            resample(
                (scw, sch),
                (cw, ch),
                |x, y| [u32::from(uv[y * stride + 2 * x]), u32::from(uv[y * stride + 2 * x + 1])],
                |x, y, [u, v]| {
                    u_out[y * cw + x] = u as u8;
                    v_out[y * cw + x] = v as u8;
                },
            );
        }
    }
}

/// Write an I420 picture (a decoded one, or my own preview) as opaque RGBA of `size` into
/// `rgba` (`size.0 × size.1 × 4` bytes, e.g. inside `FrameWriter::write`), scaling as needed.
pub fn i420_to_rgba(src: &impl YUVSource, size: (u32, u32), rgba: &mut [u8]) {
    let (sw, sh) = src.dimensions();
    let (dw, dh) = (size.0 as usize, size.1 as usize);
    let (ys, us, vs) = src.strides();
    let (y, u, v) = (src.y(), src.u(), src.v());
    if sw == 0 || sh == 0 || rgba.len() < dw * dh * 4 {
        return;
    }
    resample(
        (sw, sh),
        (dw, dh),
        |x, r| [u32::from(y[r * ys + x]), u32::from(u[r / 2 * us + x / 2]), u32::from(v[r / 2 * vs + x / 2])],
        |x, r, yuv| rgba[(r * dw + x) * 4..][..4].copy_from_slice(&rgb_of(yuv)),
    );
}

/// BT.601 limited-range luma of 8-bit R, G, B.
fn luma([r, g, b]: [u32; 3]) -> u32 {
    ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16
}

/// BT.601 limited-range (U, V) of 8-bit R, G, B.
fn chroma([r, g, b]: [u32; 3]) -> (u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u as u8, v as u8)
}

/// Opaque RGBA of a BT.601 limited-range Y, U, V.
fn rgb_of([y, u, v]: [u32; 3]) -> [u8; 4] {
    let (c, d, e) = (298 * (y as i32 - 16), u as i32 - 128, v as i32 - 128);
    let clamp = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [clamp(c + 409 * e), clamp(c - 100 * d - 208 * e), clamp(c + 516 * d), 255]
}

/// 65536 / n, rounded, for averaging n ≤ 16 taps without a division.
const RECIP: [u32; 17] = {
    let mut t = [0; 17];
    let mut n = 1;
    while n < 17 {
        t[n] = (65536 + n as u32 / 2) / n as u32;
        n += 1;
    }
    t
};

/// Area mapping of `n` destination samples onto `s` source samples (32.32 fixed point).
#[derive(Clone, Copy)]
struct Spans {
    s: usize,
    step: u64,
}

impl Spans {
    fn new(s: usize, n: usize) -> Spans {
        Spans { s, step: ((s as u64) << 32) / n.max(1) as u64 }
    }

    /// Source span of destination sample `d` as (first, taps, tap step in 16.16): every sample
    /// of spans up to 4, else 4 evenly spaced ones; at least one (upscaling repeats samples).
    fn at(self, d: usize) -> (usize, usize, usize) {
        let a = ((d as u64 * self.step) >> 32) as usize;
        let b = ((((d + 1) as u64 * self.step) >> 32) as usize).clamp(a + 1, self.s.max(a + 1));
        let len = b - a;
        if len <= 4 { (a, len, 1 << 16) } else { (a, 4, len << 14) }
    }
}

/// Box-filter `src` samples (`get(x, y)`) onto `dst` samples (`put(x, y, average)`), with at
/// most 4×4 taps per output sample. No allocation; costs about one read per source sample.
fn resample<const N: usize>(
    src: (usize, usize),
    dst: (usize, usize),
    get: impl Fn(usize, usize) -> [u32; N],
    mut put: impl FnMut(usize, usize, [u32; N]),
) {
    if src.0 == 0 || src.1 == 0 {
        return;
    }
    let (xs, ys) = (Spans::new(src.0, dst.0), Spans::new(src.1, dst.1));
    for dy in 0..dst.1 {
        let (y0, ny, ystep) = ys.at(dy);
        for dx in 0..dst.0 {
            let (x0, nx, xstep) = xs.at(dx);
            let mut sum = [0u32; N];
            for j in 0..ny {
                let y = y0 + ((j * ystep) >> 16);
                for i in 0..nx {
                    let s = get(x0 + ((i * xstep) >> 16), y);
                    for (acc, v) in sum.iter_mut().zip(s) {
                        *acc += v;
                    }
                }
            }
            let r = RECIP[nx * ny];
            put(dx, dy, sum.map(|s| (s * r + 0x8000) >> 16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Id;
    use crate::voice::video::{VideoFrames, VideoKey};

    #[test]
    fn fit_within_keeps_aspect_even_and_never_upscales() {
        assert_eq!(fit_within(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit_within(1000, 1000, 1280, 720), (720, 720));
        assert_eq!(fit_within(640, 480, 1280, 720), (640, 480), "never upscales");
        assert_eq!(fit_within(2560, 1080, 1920, 1080), (1920, 810));
        assert_eq!(fit_within(1366, 768, 1280, 720), (1280, 718), "even, rounded down");
        assert_eq!(fit_within(3001, 1001, 3840, 2160), (3000, 1000));
        assert_eq!(fit_within(1, 1, 1280, 720), (2, 2));
        assert_eq!(fit_within(0, 0, 1280, 720), (2, 2));
        assert_eq!(fit_within(10_000, 10, 1920, 1080), (1920, 2));
    }

    fn bgra(w: usize, h: usize, stride: usize, px: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut data = vec![0xEE; stride * h];
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = px(x, y);
                data[y * stride + x * 4..][..4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        data
    }

    fn near(a: u8, b: u8, tol: u8) -> bool {
        a.abs_diff(b) <= tol
    }

    #[test]
    fn colours_convert_like_bt601() {
        for (rgb, yuv) in [([255, 255, 255], [235, 128, 128]), ([0, 0, 0], [16, 128, 128]), ([255, 0, 0], [82, 90, 240]), ([0, 0, 255], [41, 240, 110])] {
            let rgb32 = rgb.map(u32::from);
            assert_eq!([luma(rgb32) as u8, chroma(rgb32).0, chroma(rgb32).1], yuv, "{rgb:?}");
            let back = rgb_of(yuv.map(u32::from));
            assert!((0..3).all(|i| near(back[i], rgb[i], 2)) && back[3] == 255, "{rgb:?} -> {back:?}");
        }
    }

    #[test]
    fn bgra_with_stride_converts_and_scales() {
        // Left half red, right half blue; 8 bytes of row padding that must be ignored.
        let (w, h, stride) = (8, 4, 8 * 4 + 8);
        let data = bgra(w, h, stride, |x, _| if x < 4 { [255, 0, 0] } else { [0, 0, 255] });
        let frame = RawFrame { width: w as u32, height: h as u32, stride, format: PixelFormat::Bgra, data: &data };

        let mut same = I420::new(8, 4);
        to_i420(&frame, &mut same);
        assert_eq!(&same.y()[..8], &[82, 82, 82, 82, 41, 41, 41, 41]);
        assert_eq!(same.u(), &[90, 90, 240, 240, 90, 90, 240, 240]);
        assert_eq!(same.v(), &[240, 240, 110, 110, 240, 240, 110, 110]);

        let mut half = I420::new(4, 2);
        to_i420(&frame, &mut half);
        assert_eq!(half.y(), &[82, 82, 41, 41, 82, 82, 41, 41], "2×2 boxes");
        assert_eq!((half.u(), half.v()), (&[90, 240][..], &[240, 110][..]));

        // A box across the red/blue edge (red for x < 3) gets the mix.
        let edge = bgra(8, 2, 32, |x, _| if x < 3 { [255, 0, 0] } else { [0, 0, 255] });
        let mut mixed = I420::new(4, 2);
        to_i420(&RawFrame { width: 8, height: 2, stride: 32, format: PixelFormat::Bgra, data: &edge }, &mut mixed);
        assert_eq!(&mixed.y()[..4], &[82, 62, 41, 41], "(82 + 41) / 2 rounded");

        // Too short for its size: ignored.
        let mut kept = I420::new(8, 4);
        to_i420(&RawFrame { data: &data[..stride * 3], ..frame }, &mut kept);
        assert!(kept.y().iter().all(|&v| v == 0));
    }

    #[test]
    fn nv12_converts_and_scales() {
        // 4×4 NV12 with stride 6: Y = 16 * (x + 1); chroma (U, V) = (50, 200) left, (100, 150) right.
        let stride = 6;
        let mut data = vec![0u8; stride * 6];
        for y in 0..4 {
            for x in 0..4 {
                data[y * stride + x] = 16 * (x as u8 + 1);
            }
        }
        for y in 0..2 {
            data[stride * 4 + y * stride..][..4].copy_from_slice(&[50, 200, 100, 150]);
        }
        let frame = RawFrame { width: 4, height: 4, stride, format: PixelFormat::Nv12, data: &data };

        let mut same = I420::new(4, 4);
        to_i420(&frame, &mut same);
        assert_eq!(&same.y()[..4], &[16, 32, 48, 64]);
        assert_eq!((same.u(), same.v()), (&[50, 100, 50, 100][..], &[200, 150, 200, 150][..]));

        let mut half = I420::new(2, 2);
        to_i420(&frame, &mut half);
        assert_eq!(half.y(), &[24, 56, 24, 56]);
        assert_eq!((half.u(), half.v()), (&[75][..], &[175][..]));

        let mut kept = I420::new(2, 2);
        to_i420(&RawFrame { data: &data[..stride * 5], ..frame }, &mut kept);
        assert!(kept.y().iter().all(|&v| v == 0), "too short: ignored");
    }

    #[test]
    fn big_ratios_and_upscales_stay_in_bounds() {
        let data = bgra(64, 36, 64 * 4, |x, y| [(x * 4) as u8, (y * 7) as u8, 9]);
        let frame = RawFrame { width: 64, height: 36, stride: 64 * 4, format: PixelFormat::Bgra, data: &data };
        for (w, h) in [(2, 2), (10, 6), (128, 72), (64, 36)] {
            let mut out = I420::new(w, h);
            to_i420(&frame, &mut out);
            assert!(out.y().iter().all(|&v| (16..=235).contains(&v)));
            let mut rgba = vec![0; (w * h * 4) as usize];
            i420_to_rgba(&out, (w, h), &mut rgba);
            assert!(rgba.chunks(4).all(|p| p[3] == 255));
        }
    }

    /// A synthetic picture: colour bars over a gradient.
    fn test_picture(w: usize, h: usize) -> Vec<u8> {
        const BARS: [[u8; 3]; 4] = [[200, 40, 40], [40, 200, 40], [40, 40, 200], [220, 220, 220]];
        bgra(w, h, w * 4, |x, y| if y < h / 2 { BARS[x * 4 / w] } else { [(x * 255 / w) as u8; 3] })
    }

    #[test]
    fn encode_decode_round_trip() {
        let (w, h) = (320, 180);
        let src = test_picture(w, h);
        let frame = RawFrame { width: w as u32, height: h as u32, stride: w * 4, format: PixelFormat::Bgra, data: &src };
        let mut pic = I420::new(w as u32, h as u32);
        to_i420(&frame, &mut pic);

        let limits = SendLimits { max_width: 640, max_height: 360, fps: 30, max_bitrate: 2_500_000 };
        let mut enc = Encoder::new(w as u32, h as u32, limits, false).unwrap();
        let t0 = Instant::now();
        let at = |n: u64| t0 + std::time::Duration::from_millis(33 * n);
        let first = enc.encode(&pic, false, t0).unwrap().expect("the first frame is encoded");
        assert!(first.keyframe && first.captured == t0);
        assert!(first.annexb.starts_with(&[0, 0, 0, 1]));
        let start_codes = first.annexb.windows(3).filter(|w| w == &[0, 0, 1]).count();
        let four_byte = first.annexb.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        assert_eq!(start_codes, four_byte, "4-byte start codes only");
        // SPS (constrained baseline: profile_idc 66, constraint_set1) and PPS come before the IDR.
        let types: Vec<u8> = first.annexb.windows(5).filter(|w| w[..4] == [0, 0, 0, 1]).map(|w| w[4] & 0x1f).collect();
        assert_eq!(&types[..2], &[7, 8]);
        assert!(types.contains(&5));
        let sps = &first.annexb[4..];
        assert_eq!((sps[1], sps[2] & 0x40), (66, 0x40), "Constrained Baseline");

        let frames = VideoFrames::default();
        let key = VideoKey::User(Id(1));
        let writer = frames.writer(key.clone()).unwrap();
        let mut dec = Decoder::new().unwrap();
        assert_eq!(dec.decode(&first.annexb, &writer).unwrap(), Some((w as u32, h as u32)));

        // P frames follow; a forced keyframe is an IDR again (with the next encoded frame if
        // the rate control skips one).
        let p = enc.encode(&pic, false, at(1)).unwrap();
        if let Some(p) = &p {
            assert!(!p.keyframe);
            assert_eq!(dec.decode(&p.annexb, &writer).unwrap(), Some((w as u32, h as u32)));
        }
        let mut forced = None;
        for n in 2..12 {
            if let Some(f) = enc.encode(&pic, n == 2, at(n)).unwrap() {
                assert_eq!(dec.decode(&f.annexb, &writer).unwrap(), Some((w as u32, h as u32)));
                forced = Some(f);
                break;
            }
        }
        let forced = forced.expect("an encoded frame");
        assert!(forced.keyframe && forced.annexb[4] & 0x1F == 7, "a forced IDR comes with its SPS/PPS");

        let (_, image) = frames.latest(&key).unwrap();
        assert_eq!(image.size, [w, h]);
        // Colours survive within codec tolerance (centres of the bars and the gradient).
        let at = |x: usize, y: usize| image.pixels[y * w + x];
        for (x, y) in [(40, 45), (120, 45), (200, 45), (280, 45), (20, 135), (300, 135)] {
            let want = &src[(y * w + x) * 4..][..3]; // B, G, R
            let got = at(x, y);
            assert!(near(got.r(), want[2], 16) && near(got.g(), want[1], 16) && near(got.b(), want[0], 16), "({x},{y}): {got:?} vs {want:?}");
            assert_eq!(got.a(), 255);
        }

        // A slice without its SPS/PPS (joined mid-stream) is an error: the caller asks for a keyframe.
        let idr_at = first.annexb.windows(5).position(|w| w == [0, 0, 0, 1, 0x65]).unwrap();
        assert!(Decoder::new().unwrap().decode(&first.annexb[idr_at..], &writer).is_err());
        assert!(Encoder::new(321, 180, limits, false).is_err());
        assert!(Encoder::new(4096, 2160, limits, false).is_err());
    }

    #[test]
    fn oversized_pictures_are_downscaled_for_the_ui() {
        let (w, h) = (2560, 1440);
        let limits = SendLimits { max_width: w, max_height: h, fps: 5, max_bitrate: 4_000_000 };
        let mut enc = Encoder::new(w, h, limits, true).unwrap();
        let mut pic = I420::new(w, h);
        let src = test_picture(w as usize, h as usize);
        to_i420(&RawFrame { width: w, height: h, stride: w as usize * 4, format: PixelFormat::Bgra, data: &src }, &mut pic);
        let idr = enc.encode(&pic, false, Instant::now()).unwrap().unwrap();

        let frames = VideoFrames::default();
        let writer = frames.writer(VideoKey::Stream("call:1:2".into())).unwrap();
        assert_eq!(Decoder::new().unwrap().decode(&idr.annexb, &writer).unwrap(), Some((1920, 1080)));
        let image = frames.latest(&VideoKey::Stream("call:1:2".into())).unwrap().1;
        assert_eq!(image.size, [1920, 1080]);
        let px = image.pixels[200 * 1920 + 240]; // first bar
        assert!(near(px.r(), 200, 16) && near(px.g(), 40, 16) && near(px.b(), 40, 16), "{px:?}");
    }
}

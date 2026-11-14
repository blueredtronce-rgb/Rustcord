//! Pure MP4 / H.264 / AAC helpers of the player (unit-tested): avcC -> Annex-B, AAC-LC config,
//! track picking, keyframe and audio sample lookup for seeks, and the top-level box budget that
//! runs before re_mp4 parses anything. docs/MEDIA_DESIGN.md §4.

use super::Fail;
use re_mp4::{Mp4, StsdBoxContent, TrackKind};
use std::io::{Read, Seek, SeekFrom};

/// Header boxes (everything but `mdat` / `free` / `skip`) larger than this are refused before
/// parsing: re_mp4 allocates box contents up front, so a hostile size must never reach it.
pub const HEADER_BUDGET: u64 = 32 << 20;
/// Largest picture rate decoded here (openh264 C path, one core): 1080p60.
const MAX_PIXEL_RATE: u64 = 1920 * 1080 * 60;

/// One sample of a track, in the track's timescale.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct S {
    pub offset: u64,
    pub size: u32,
    pub sync: bool,
    /// Presentation time (first one is 0).
    pub cts: i64,
    pub dur: u64,
}

pub struct Video {
    pub samples: Vec<S>,
    pub timescale: u64,
    pub nal_len: usize,
    /// SPS + PPS as Annex-B, fed before the first sample after every (re)start.
    pub params: Vec<u8>,
    pub size: (u32, u32),
}

pub struct Audio {
    pub samples: Vec<S>,
    pub timescale: u64,
    /// AAC-LC AudioSpecificConfig.
    pub asc: [u8; 2],
}

pub struct Tracks {
    pub video: Video,
    pub audio: Option<Audio>,
    pub duration_ms: u64,
}

/// avcC -> (NAL length size, SPS+PPS as Annex-B). None unless configurationVersion == 1.
pub fn avcc_params(avcc: &[u8]) -> Option<(usize, Vec<u8>)> {
    if avcc.first() != Some(&1) {
        return None;
    }
    let nal_len = usize::from(avcc.get(4)? & 3) + 1;
    let (mut out, mut i) = (Vec::new(), 5);
    // numOfSPS (5 bits), then numOfPPS (8 bits).
    for mask in [0x1f, 0xff] {
        let n = usize::from(avcc.get(i)? & mask);
        i += 1;
        for _ in 0..n {
            let l = usize::from(u16::from_be_bytes([*avcc.get(i)?, *avcc.get(i + 1)?]));
            i += 2;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(avcc.get(i..i + l)?);
            i += l;
        }
    }
    Some((nal_len, out))
}

/// Length-prefixed NAL units of one sample -> Annex-B appended to `out`; None if truncated.
pub fn to_annexb(sample: &[u8], nal_len: usize, out: &mut Vec<u8>) -> Option<()> {
    let mut i = 0;
    while i < sample.len() {
        let n = sample.get(i..i + nal_len)?.iter().fold(0usize, |a, &b| (a << 8) | usize::from(b));
        i += nal_len;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(sample.get(i..i.checked_add(n)?)?);
        i += n;
    }
    Some(())
}

/// AAC-LC AudioSpecificConfig from re_mp4's parsed esds (AOT 5/29, HE-AAC, -> their LC core).
pub fn lc_asc(aot: u8, freq_index: u8, chan_conf: u8) -> Option<[u8; 2]> {
    (matches!(aot, 2 | 5 | 29) && freq_index < 13 && matches!(chan_conf, 1 | 2)).then_some([(2 << 3) | (freq_index >> 1), ((freq_index & 1) << 7) | (chan_conf << 3)])
}

/// Where decoding starts for a seek to `t` (track ticks): the last sync sample, in decode order,
/// at or before the sample with the largest presentation time ≤ `t`.
pub fn keyframe_for(samples: &[S], t: i64) -> usize {
    let shown = samples.iter().enumerate().filter(|(_, s)| s.cts <= t).max_by_key(|(_, s)| s.cts).map_or(0, |(i, _)| i);
    samples[..=shown.min(samples.len().saturating_sub(1))].iter().rposition(|s| s.sync).unwrap_or(0)
}

/// The first audio sample still playing at `t` (track ticks).
pub fn audio_index_for(samples: &[S], t: i64) -> usize {
    samples.iter().position(|s| s.cts + s.dur as i64 > t).unwrap_or(samples.len())
}

/// Walks the top-level boxes (reading only their 8/16-byte headers) and refuses a file whose
/// header boxes exceed `HEADER_BUDGET`, or whose boxes overrun it.
pub fn check_boxes(r: &mut (impl Read + Seek), len: u64) -> Result<(), Fail> {
    let (mut at, mut header) = (0u64, 0u64);
    while at + 8 <= len {
        r.seek(SeekFrom::Start(at)).map_err(|_| Fail::Network)?;
        let mut h = [0u8; 16];
        r.read_exact(&mut h[..8]).map_err(|_| Fail::Network)?;
        let mut size = u64::from(u32::from_be_bytes([h[0], h[1], h[2], h[3]]));
        if size == 1 {
            r.read_exact(&mut h[8..]).map_err(|_| Fail::Network)?;
            size = u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
        } else if size == 0 {
            // "To the end of the file".
            size = len - at;
        }
        if size < 8 || size > len - at {
            return Err(Fail::Unsupported);
        }
        if !matches!(&h[4..8], b"mdat" | b"free" | b"skip") {
            header += size;
            if header > HEADER_BUDGET {
                return Err(Fail::Unsupported);
            }
        }
        at += size;
    }
    Ok(())
}

/// The first H.264 track and the first playable AAC track. Refuses other codecs (HEVC, VP9,
/// AV1 …) and more than 1080p60.
pub fn pick_tracks(mp4: &Mp4) -> Result<Tracks, Fail> {
    let own = |t: &re_mp4::Track| -> Vec<S> {
        t.samples.iter().map(|s| S { offset: s.offset, size: u32::try_from(s.size).unwrap_or(u32::MAX), sync: s.is_sync, cts: s.composition_timestamp, dur: s.duration }).collect()
    };
    let mut video = None;
    let mut audio = None;
    for t in mp4.tracks().values() {
        let stsd = &t.trak(mp4).mdia.minf.stbl.stsd.contents;
        match (t.kind, stsd) {
            (Some(TrackKind::Video), StsdBoxContent::Avc1(_)) if video.is_none() => {
                let raw = t.raw_codec_config(mp4).unwrap_or_default();
                let (nal_len, params) = avcc_params(&raw).ok_or(Fail::Unsupported)?;
                let samples = own(t);
                let secs = t.duration as f64 / t.timescale.max(1) as f64;
                let fps = if secs > 0.0 { samples.len() as f64 / secs } else { 30.0 };
                if u64::from(t.width) * u64::from(t.height) * fps.ceil() as u64 > MAX_PIXEL_RATE || samples.is_empty() {
                    return Err(Fail::TooLarge);
                }
                video = Some(Video { samples, timescale: t.timescale.max(1), nal_len, params, size: (u32::from(t.width), u32::from(t.height)) });
            }
            (Some(TrackKind::Audio), StsdBoxContent::Mp4a(m)) if audio.is_none() => {
                let d = m.esds.as_ref().map(|e| &e.es_desc.dec_config.dec_specific);
                if let Some(asc) = d.and_then(|d| lc_asc(d.profile, d.freq_index, d.chan_conf)) {
                    audio = Some(Audio { samples: own(t), timescale: t.timescale.max(1), asc });
                }
            }
            _ => {}
        }
    }
    let video: Video = video.ok_or(Fail::Unsupported)?;
    let end = video.samples.iter().map(|s| s.cts + s.dur as i64).max().unwrap_or(0).max(0) as u64;
    Ok(Tracks { duration_ms: end * 1000 / video.timescale, video, audio })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const DEMO: &[u8] = include_bytes!("../../tests/fixtures/media/demo.mp4");

    #[test]
    fn avcc_of_the_fixture_becomes_annexb() {
        let mp4 = Mp4::read_bytes(DEMO).unwrap();
        let t = mp4.tracks().values().find(|t| t.kind == Some(TrackKind::Video)).unwrap();
        let raw = t.raw_codec_config(&mp4).unwrap();
        assert_eq!((raw.len(), raw[1]), (43, 100), "43 B avcC, High profile");
        let (nal_len, params) = avcc_params(&raw).unwrap();
        assert_eq!(nal_len, 4);
        assert_eq!(params.windows(4).filter(|w| w == &[0, 0, 0, 1]).count(), 2, "SPS and PPS");
        assert_eq!(params[4] & 0x1f, 7, "SPS first");
        let mut bad = raw.clone();
        bad[0] = 2;
        assert_eq!(avcc_params(&bad), None);
        assert_eq!(avcc_params(&raw[..20]), None, "truncated");
    }

    #[test]
    fn length_prefixed_nals_to_annexb() {
        let mut out = Vec::new();
        to_annexb(&[0, 2, 0x65, 1, 0, 1, 0x41], 2, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41]);
        out.clear();
        to_annexb(&[0, 0, 0, 1, 9], 4, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 9]);
        assert_eq!(to_annexb(&[0, 0, 0, 5, 9], 4, &mut Vec::new()), None, "truncated NAL");
        assert_eq!(to_annexb(&[0, 0], 4, &mut Vec::new()), None, "truncated length");
    }

    #[test]
    fn aac_lc_config() {
        assert_eq!(lc_asc(2, 3, 2), Some([0x11, 0x90]), "LC 48 kHz stereo");
        assert_eq!(lc_asc(5, 3, 2), lc_asc(2, 3, 2), "HE-AAC plays its LC core");
        assert_eq!(lc_asc(2, 3, 6), None, "5.1 stays muted");
        assert_eq!(lc_asc(1, 3, 2), None);
        assert_eq!(lc_asc(2, 13, 2), None);
    }

    #[test]
    fn seeks_start_at_a_keyframe_before_the_target() {
        // Decode order I P B B P B B | I P B B, 10 ticks per frame, B-frames shown before their P.
        let at = |cts: i64, sync: bool| S { cts, sync, dur: 10, ..S::default() };
        let s = [at(0, true), at(30, false), at(10, false), at(20, false), at(60, false), at(40, false), at(50, false), at(70, true), at(100, false), at(80, false), at(90, false)];
        assert_eq!(keyframe_for(&s, 0), 0);
        assert_eq!(keyframe_for(&s, 55), 0, "frame 50 is decoded after the first keyframe");
        assert_eq!(keyframe_for(&s, 75), 7);
        assert_eq!(keyframe_for(&s, 1000), 7);
        assert_eq!(keyframe_for(&s, -5), 0);
        assert_eq!(audio_index_for(&s[..4], 25), 1, "first sample in display order still playing");
        assert_eq!(audio_index_for(&[at(0, true), at(10, true)], 15), 1);
        assert_eq!(audio_index_for(&[at(0, true)], 50), 1, "past the end");
    }

    #[test]
    fn box_budget_runs_before_parsing() {
        assert_eq!(check_boxes(&mut Cursor::new(DEMO), DEMO.len() as u64), Ok(()));
        let boxed = |name: &[u8; 4], size: u32| {
            let mut b = size.to_be_bytes().to_vec();
            b.extend_from_slice(name);
            b
        };
        // A moov claiming 2 GB in a small file.
        let mut f = boxed(b"ftyp", 8);
        f.extend(boxed(b"moov", 2_000_000_000));
        f.resize(4096, 0);
        assert_eq!(check_boxes(&mut Cursor::new(&f), f.len() as u64), Err(Fail::Unsupported));
        // A 33 MiB moov inside a file that is big enough: over the header budget.
        let mut f = boxed(b"moov", 33 << 20);
        f.resize(8, 0);
        assert_eq!(check_boxes(&mut Cursor::new(&f), (33 << 20) + 8), Err(Fail::Unsupported));
        // A huge mdat is fine (it is skipped by seeking).
        let f = boxed(b"mdat", 900 << 20);
        assert_eq!(check_boxes(&mut Cursor::new(&f), 900 << 20), Ok(()));
    }

    #[test]
    fn fixture_tracks() {
        let t = pick_tracks(&Mp4::read_bytes(DEMO).unwrap()).unwrap();
        assert_eq!((t.video.size, t.video.samples.len()), ((640, 360), 90));
        assert!(t.video.samples[0].sync);
        assert_eq!(t.audio.as_ref().map(|a| a.asc), Some([0x11, 0x90]));
        assert!((2950..=3100).contains(&t.duration_ms), "{}", t.duration_ms);
    }
}

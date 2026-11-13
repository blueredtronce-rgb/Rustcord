//! H.264 over RTP (RFC 6184, VOICE_NOTES §5.3): packetizing (single NAL / STAP-A / FU-A), RTX
//! unwrapping, receive-side reordering, depacketizing (single NAL / STAP-A / FU-A) and frame
//! assembly into Annex-B with 4-byte start codes (DAVE needs them), the sender's RTX history,
//! RTCP NACK/PLI, and the video RTP header with its extensions. Pure, no I/O.
//!
//! OWNERSHIP: codec/RTP engineer. Keep the pub signatures (receive.rs and
//! voice/transport/video.rs use them).

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// H.264 bytes per RTP packet (VOICE_NOTES §5.3: ~1100–1200 byte payloads).
pub const MAX_PAYLOAD: usize = 1100;
/// Sent packets kept for retransmission on NACK.
pub const RTX_HISTORY: Duration = Duration::from_secs(1);
/// Packets a `Depacketizer` holds while waiting for a gap to fill.
pub const REORDER_PACKETS: usize = 256;
/// Packets of the largest frame a `Depacketizer` assembles (~2.4 MB, a 4K keyframe), on top.
const MAX_FRAME_PACKETS: usize = 2048;

/// A gap is NACKed once it is this old (plain reordering fills it before)…
const NACK_DELAY: Duration = Duration::from_millis(20);
/// …then again after this long, at most `NACK_TRIES` times…
const NACK_INTERVAL: Duration = Duration::from_millis(100);
const NACK_TRIES: u8 = 3;
/// …and is lost for good (its frame is dropped, a keyframe requested) once this old.
const LOSS_TIMEOUT: Duration = Duration::from_millis(350);
/// At most one keyframe request (PLI) per this long.
const PLI_INTERVAL: Duration = Duration::from_millis(500);
/// Sent packets kept at most (1 s at 4 Mbit/s is ~460).
const RTX_PACKETS: usize = 1024;

const STAP_A: u8 = 24;
const FU_A: u8 = 28;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One received video RTP packet: transport-decrypted, extensions and padding removed. For
/// RTX (PT 104) the caller maps the RTX SSRC to the video SSRC and `unwrap_rtx` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    pub ssrc: u32,
    pub seq: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: &'a [u8],
}

/// NAL units of an Annex-B stream (3- or 4-byte start codes), without start codes; bytes
/// before the first start code are skipped.
fn nal_units(annexb: &[u8]) -> impl Iterator<Item = &[u8]> {
    let find = |b: &[u8]| b.windows(3).position(|w| w == [0, 0, 1]);
    let mut rest = find(annexb).map_or(&[][..], |i| &annexb[i..]);
    std::iter::from_fn(move || {
        while rest.len() > 3 {
            let body = &rest[3..];
            let end = find(body).unwrap_or(body.len());
            let mut nal = &body[..end];
            rest = &body[end..];
            // Only the first zero of a following 4-byte start code, like WebRTC: a DAVE
            // ciphertext may end in 0 (davey allows it there), the receiver must get it back.
            if let (false, [head @ .., 0]) = (rest.is_empty(), nal) {
                nal = head;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
        None
    })
}

/// One access unit (Annex-B, after DAVE) → RTP payloads in order: runs of NAL units that fit
/// together go as one STAP-A (SPS + PPS + a small slice), a single one that fits goes whole,
/// larger ones as FU-A (indicator `F|NRI|28`, header `S|E|0|type`). The caller sets the marker
/// bit on the last payload; all share the frame's timestamp.
pub fn packetize(annexb: &[u8], max_payload: usize) -> Vec<Vec<u8>> {
    let max = max_payload.clamp(3, usize::from(u16::MAX));
    let mut packets = Vec::new();
    let mut group: Vec<&[u8]> = Vec::new();
    let mut group_len = 1; // STAP-A header
    for nal in nal_units(annexb) {
        if !group.is_empty() && group_len + 2 + nal.len() > max {
            packets.push(aggregate(&group));
            group.clear();
            group_len = 1;
        }
        if nal.len() <= max {
            group.push(nal);
            group_len += 2 + nal.len();
        } else {
            fragment(nal, max, &mut packets);
        }
    }
    if !group.is_empty() {
        packets.push(aggregate(&group));
    }
    packets
}

/// One NAL unit as is, several as a STAP-A (F = any F, NRI = the highest).
fn aggregate(nals: &[&[u8]]) -> Vec<u8> {
    if let [nal] = nals {
        return nal.to_vec();
    }
    let f = nals.iter().fold(0, |f, n| f | (n[0] & 0x80));
    let nri = nals.iter().map(|n| n[0] & 0x60).max().unwrap_or(0);
    let mut p = Vec::with_capacity(1 + nals.iter().map(|n| 2 + n.len()).sum::<usize>());
    p.push(f | nri | STAP_A);
    for n in nals {
        p.extend_from_slice(&(n.len() as u16).to_be_bytes());
        p.extend_from_slice(n);
    }
    p
}

/// FU-A fragments of a NAL unit longer than `max` (so always ≥ 2 fragments).
fn fragment(nal: &[u8], max: usize, out: &mut Vec<Vec<u8>>) {
    let (header, body) = (nal[0], &nal[1..]);
    let count = body.len().div_ceil(max - 2);
    for (i, chunk) in body.chunks(max - 2).enumerate() {
        let start = if i == 0 { 0x80 } else { 0 };
        let end = if i + 1 == count { 0x40 } else { 0 };
        let mut p = Vec::with_capacity(2 + chunk.len());
        p.extend_from_slice(&[(header & 0xE0) | FU_A, start | end | (header & 0x1F)]);
        p.extend_from_slice(chunk);
        out.push(p);
    }
}

/// RTX payload (PT 104) → (original sequence number, original payload). None for padding-only
/// packets (bandwidth probes) and anything shorter.
pub fn unwrap_rtx(payload: &[u8]) -> Option<(u16, &[u8])> {
    match payload {
        [a, b, rest @ ..] if !rest.is_empty() => Some((u16::from_be_bytes([*a, *b]), rest)),
        _ => None,
    }
}

/// Receive state of one video SSRC: reorder by u16 sequence (wraparound), assemble a frame by
/// timestamp until the marker, notice losses. Bounded by `REORDER_PACKETS` + one frame.
///
/// Frames come out whole and in order, starting with a keyframe: at the start, after a frame
/// was lost (a gap stayed open `LOSS_TIMEOUT`) and after `wait_for_keyframe`, frames are
/// dropped until one with an IDR slice arrives, and `take_keyframe_request` asks for it. A
/// frame is never cut short or glued to another (DAVE authenticates whole frames).
#[derive(Default)]
pub struct Depacketizer {
    /// Buffered packets by extended sequence number.
    packets: BTreeMap<u64, Packet>,
    /// Sequence numbers below `highest` not received yet.
    missing: BTreeMap<u64, Missing>,
    /// Highest extended sequence number seen.
    highest: Option<u64>,
    /// Everything below is done with (taken, skipped or given up).
    floor: u64,
    /// First sequence number of the next frame, when known (after a frame was taken/skipped).
    next: Option<u64>,
    /// False until a keyframe went out (again).
    synced: bool,
    last_pli: Option<Instant>,
}

struct Packet {
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
}

struct Missing {
    since: Instant,
    tries: u8,
    last_nack: Option<Instant>,
    /// Gave up on it (`LOSS_TIMEOUT`).
    lost: bool,
}

impl Depacketizer {
    /// Feed one packet (RTX already unwrapped: `seq` = the original sequence number).
    pub fn push(&mut self, p: RtpPacket<'_>, now: Instant) {
        if p.payload.is_empty() {
            return;
        }
        // Extended sequence number: the u16 step from the highest one, wraparound included.
        let seq = match self.highest {
            None => (1 << 32) | u64::from(p.seq),
            Some(h) => h.wrapping_add_signed(i64::from(p.seq.wrapping_sub(h as u16) as i16)),
        };
        // One window everywhere, as large as the buffer: a gap in a big keyframe stays NACKable
        // (and its RTX accepted) until `LOSS_TIMEOUT` or until its frame is done with.
        let window = (REORDER_PACKETS + MAX_FRAME_PACKETS) as u64;
        match self.highest {
            Some(h) if seq > h + window => {
                // A jump (sender restarted, long outage): start over.
                *self = Depacketizer { last_pli: self.last_pli, ..Depacketizer::default() };
                self.highest = Some(seq);
            }
            Some(h) if seq > h => {
                for m in h + 1..seq {
                    self.missing.insert(m, Missing { since: now, tries: 0, last_nack: None, lost: false });
                }
                self.highest = Some(seq);
            }
            Some(h) if seq + window <= h => return,
            Some(_) => {
                if self.missing.remove(&seq).is_none() && seq >= self.floor && !self.packets.contains_key(&seq) {
                    // Older than anything seen yet (reordered at the start): the ones between are missing too.
                    let low = self.packets.keys().next().into_iter().chain(self.missing.keys().next()).min().copied();
                    for m in seq + 1..low.unwrap_or(seq) {
                        self.missing.insert(m, Missing { since: now, tries: 0, last_nack: None, lost: false });
                    }
                }
            }
            None => self.highest = Some(seq),
        }
        if seq < self.floor || self.packets.contains_key(&seq) {
            return;
        }
        self.packets.insert(seq, Packet { timestamp: p.timestamp, marker: p.marker, payload: p.payload.to_vec() });
        let highest = self.highest.unwrap_or(seq);
        while self.missing.first_key_value().is_some_and(|(&m, _)| m + window <= highest) {
            self.missing.pop_first();
        }
        while self.packets.len() > REORDER_PACKETS + MAX_FRAME_PACKETS {
            // Nothing completes: give the oldest up.
            if let Some((oldest, _)) = self.packets.pop_first() {
                self.drop_below(oldest + 1);
            }
            self.next = None;
            self.synced = false;
        }
        self.expire(now);
    }

    /// The next complete access unit (Annex-B, 4-byte start codes), in order.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let highest = self.highest?;
        loop {
            let start = match self.next {
                Some(n) => n,
                None => self.find_start()?,
            };
            // Consecutive packets of one timestamp until the marker (or a new timestamp).
            let (mut expect, mut timestamp, mut end) = (start, None, None);
            for (&seq, p) in self.packets.range(start..) {
                if seq != expect {
                    break;
                }
                if timestamp.is_some_and(|t| t != p.timestamp) {
                    end = Some(seq - 1);
                    break;
                }
                if p.marker {
                    end = Some(seq);
                    break;
                }
                timestamp = Some(p.timestamp);
                expect += 1;
            }
            let Some(end) = end else {
                if expect > highest || self.missing.get(&expect).is_some_and(|m| !m.lost) {
                    return None; // not here yet
                }
                // `expect` is lost: drop the frame; the next one starts after an unknown gap.
                self.drop_below(expect + 1);
                self.next = None;
                self.synced = false;
                continue;
            };
            let rest = self.packets.split_off(&(end + 1));
            let frame = std::mem::replace(&mut self.packets, rest);
            self.drop_below(end + 1);
            self.next = Some(end + 1);
            match assemble(frame.range(start..).map(|(_, p)| &p.payload[..])) {
                Some((annexb, key)) if key || self.synced => {
                    self.synced = true;
                    return Some(annexb);
                }
                Some(_) => {} // not decodable before a keyframe
                None => self.synced = false,
            }
        }
    }

    /// Sequence numbers to NACK now (each one asked a few times at most). Call it regularly
    /// (every ~20–50 ms while packets flow): it also gives up on old gaps.
    pub fn take_nacks(&mut self, now: Instant) -> Vec<u16> {
        self.expire(now);
        let mut nacks = Vec::new();
        for (&seq, m) in &mut self.missing {
            let due = m.last_nack.map_or(m.since + NACK_DELAY, |t| t + NACK_INTERVAL);
            if !m.lost && m.tries < NACK_TRIES && now >= due {
                m.tries += 1;
                m.last_nack = Some(now);
                nacks.push(seq as u16);
            }
        }
        nacks
    }

    /// A frame was lost for good (or nothing decodable arrived yet): send a PLI. True at most
    /// once per ~500 ms.
    pub fn take_keyframe_request(&mut self, now: Instant) -> bool {
        let due = !self.synced && self.last_pli.is_none_or(|t| now.saturating_duration_since(t) >= PLI_INTERVAL);
        if due {
            self.last_pli = Some(now);
        }
        due
    }

    /// The decoder failed on a frame: drop frames until the next keyframe and ask for one
    /// (`take_keyframe_request`).
    pub fn wait_for_keyframe(&mut self) {
        self.synced = false;
    }

    fn expire(&mut self, now: Instant) {
        for m in self.missing.values_mut() {
            m.lost |= now.saturating_duration_since(m.since) >= LOSS_TIMEOUT;
        }
    }

    /// Forget everything below `seq`.
    fn drop_below(&mut self, seq: u64) {
        self.packets = self.packets.split_off(&seq);
        self.missing = self.missing.split_off(&seq);
        self.floor = self.floor.max(seq);
    }

    /// Resynchronise: the first buffered packet that surely starts a frame (its predecessor
    /// ended one, or it opens with an SPS/AUD), unless a gap before it may still fill (it may
    /// hold a keyframe's start). Drops everything before it.
    fn find_start(&mut self) -> Option<u64> {
        let mut prev: Option<(u64, &Packet)> = None;
        let mut start = None;
        for (&seq, p) in &self.packets {
            let begins = match prev {
                Some((s, q)) if s + 1 == seq => q.marker || q.timestamp != p.timestamp,
                _ => opens_access_unit(&p.payload),
            };
            if begins {
                start = Some(seq);
                break;
            }
            prev = Some((seq, p));
        }
        let start = start?;
        if self.missing.range(..start).any(|(_, m)| !m.lost) {
            return None;
        }
        self.drop_below(start);
        self.next = Some(start);
        Some(start)
    }
}

/// Whether an RTP payload opens an access unit for sure: its first NAL unit is an access unit
/// delimiter or an SPS (keyframes start with SPS, PPS, IDR).
fn opens_access_unit(payload: &[u8]) -> bool {
    let first = match payload {
        [h, _, _, n, ..] if h & 0x1F == STAP_A => *n,
        [h, fu, ..] if h & 0x1F == FU_A && fu & 0x80 != 0 => *fu,
        [h, ..] => *h,
        [] => return false,
    };
    matches!(first & 0x1F, 7 | 9)
}

/// The payloads of one frame → (Annex-B with 4-byte start codes, has an IDR slice). None for
/// a malformed or unsupported payload (STAP-B, MTAP, FU-B, broken FU-A sequence).
fn assemble<'a>(payloads: impl Iterator<Item = &'a [u8]>) -> Option<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut key = false;
    let mut in_fu = false;
    let mut put = |out: &mut Vec<u8>, nal: &[u8]| {
        key |= nal[0] & 0x1F == 5;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    };
    for p in payloads {
        match p[0] & 0x1F {
            1..=23 if !in_fu => put(&mut out, p),
            STAP_A if !in_fu => {
                let mut rest = &p[1..];
                while let [a, b, tail @ ..] = rest {
                    let n = usize::from(u16::from_be_bytes([*a, *b]));
                    if n == 0 || n > tail.len() {
                        return None;
                    }
                    put(&mut out, &tail[..n]);
                    rest = &tail[n..];
                }
                if !rest.is_empty() {
                    return None;
                }
            }
            FU_A => {
                let [indicator, header, body @ ..] = p else { return None };
                let start = header & 0x80 != 0;
                if start == in_fu {
                    return None; // a start inside a fragmented unit, or a middle without a start
                }
                if start {
                    put(&mut out, &[(indicator & 0xE0) | (header & 0x1F)]);
                }
                out.extend_from_slice(body);
                in_fu = header & 0x40 == 0;
            }
            _ => return None,
        }
    }
    (!in_fu && !out.is_empty()).then_some((out, key))
}

/// Sender side: the payloads of the last `RTX_HISTORY` of sent packets, by sequence number.
/// Buffers of expired packets are reused (no allocation per packet once warm).
#[derive(Default)]
pub struct RtxHistory {
    sent: VecDeque<(u16, Instant, Vec<u8>)>,
}

impl RtxHistory {
    pub fn keep(&mut self, seq: u16, payload: &[u8], now: Instant) {
        let expired = |e: &(u16, Instant, Vec<u8>)| now.saturating_duration_since(e.1) > RTX_HISTORY;
        let mut buf = Vec::new();
        while self.sent.front().is_some_and(expired) || self.sent.len() >= RTX_PACKETS {
            buf = self.sent.pop_front().map(|e| e.2).unwrap_or_default();
        }
        buf.clear();
        buf.extend_from_slice(payload);
        self.sent.push_back((seq, now, buf));
    }

    pub fn get(&self, seq: u16) -> Option<&[u8]> {
        let first = self.sent.front()?.0;
        // Sequence numbers are consecutive, so the offset finds it; search otherwise.
        let hit = self.sent.get(usize::from(seq.wrapping_sub(first))).filter(|e| e.0 == seq);
        hit.or_else(|| self.sent.iter().find(|e| e.0 == seq)).map(|e| &e.2[..])
    }
}

/// RTCP feedback about a video I send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    /// Generic NACK (RTPFB, PT 205 FMT 1): resend these from the `RtxHistory` as RTX.
    Nack { media_ssrc: u32, lost: Vec<u16> },
    /// Picture Loss Indication (PSFB, PT 206 FMT 1): next frame is a keyframe.
    Pli { media_ssrc: u32 },
}

/// Plain (not yet transport-encrypted) RTCP Generic NACK for `lost` packets of `media_ssrc`
/// (in sending order, as `take_nacks` gives them). Empty when nothing is lost.
pub fn nack(sender_ssrc: u32, media_ssrc: u32, lost: &[u16]) -> Vec<u8> {
    // FCI entries: a packet ID and a bitmask of the 16 following ones.
    let mut fci: Vec<(u16, u16)> = Vec::new();
    for &seq in lost {
        match fci.last_mut() {
            Some((pid, blp)) if (1..=16).contains(&seq.wrapping_sub(*pid)) => *blp |= 1 << (seq.wrapping_sub(*pid) - 1),
            Some((pid, _)) if *pid == seq => {}
            _ => fci.push((seq, 0)),
        }
    }
    if fci.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(12 + 4 * fci.len());
    out.extend_from_slice(&[0x81, 205]);
    out.extend_from_slice(&((2 + fci.len()) as u16).to_be_bytes());
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    out.extend_from_slice(&media_ssrc.to_be_bytes());
    for (pid, blp) in fci {
        out.extend_from_slice(&pid.to_be_bytes());
        out.extend_from_slice(&blp.to_be_bytes());
    }
    out
}

/// Plain RTCP PLI for `media_ssrc`.
pub fn pli(sender_ssrc: u32, media_ssrc: u32) -> Vec<u8> {
    let mut out = vec![0x81, 206, 0, 2];
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    out.extend_from_slice(&media_ssrc.to_be_bytes());
    out
}

/// NACKs and PLIs in a decrypted (compound) RTCP packet; anything else is skipped.
pub fn parse_feedback(rtcp: &[u8]) -> Vec<Feedback> {
    let mut found = Vec::new();
    let mut rest = rtcp;
    while let [b0, pt, l0, l1, ..] = *rest {
        let len = (usize::from(u16::from_be_bytes([l0, l1])) + 1) * 4;
        if b0 >> 6 != 2 || len > rest.len() {
            break;
        }
        let (pkt, tail) = rest.split_at(len);
        rest = tail;
        let pad = if b0 & 0x20 != 0 { usize::from(pkt[len - 1]) } else { 0 };
        let Some(body) = pkt.get(..len.saturating_sub(pad)).filter(|b| b.len() >= 12 && b0 & 0x1F == 1) else { continue };
        let media_ssrc = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
        match pt {
            205 => {
                let mut lost = Vec::new();
                for &[a, b, c, d] in body[12..].as_chunks::<4>().0 {
                    let (pid, blp) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
                    lost.push(pid);
                    lost.extend((0..16).filter(|i| (blp >> i) & 1 != 0).map(|i| pid.wrapping_add(i + 1)));
                }
                if !lost.is_empty() {
                    found.push(Feedback::Nack { media_ssrc, lost });
                }
            }
            206 => found.push(Feedback::Pli { media_ssrc }),
            _ => {}
        }
    }
    found
}

/// RTP header of one outgoing video packet (PT `pt` = 103, or 104 for RTX) with the video
/// extensions (one-byte profile 0xBEDE: ID 6 playout delay, ID 11 RID "100", ID 7 content
/// type 1 when `screen`, VOICE_NOTES §5.3), replacing the contents of `out`. Returns the clear
/// length (the AEAD's AAD: header + 0xBEDE preamble; the extension elements are encrypted with
/// the payload).
pub fn write_video_header(out: &mut Vec<u8>, pt: u8, seq: u16, timestamp: u32, ssrc: u32, marker: bool, screen: bool) -> usize {
    // Elements `(id << 4) | (len - 1)` + data, zero-padded to 32 bits. Playout delay min = max
    // = 0 (render at once).
    const CAMERA: [u8; 8] = [0x62, 0, 0, 0, 0xB2, b'1', b'0', b'0'];
    const SCREEN: [u8; 12] = [0x62, 0, 0, 0, 0xB2, b'1', b'0', b'0', 0x70, 1, 0, 0];
    let ext: &[u8] = if screen { &SCREEN } else { &CAMERA };
    out.clear();
    out.extend_from_slice(&[0x90, (u8::from(marker) << 7) | (pt & 0x7F)]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(&[0xBE, 0xDE]);
    out.extend_from_slice(&((ext.len() / 4) as u16).to_be_bytes());
    let clear = out.len();
    out.extend_from_slice(ext);
    clear
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NAL unit of `len` bytes with header `header` and a body without zero bytes.
    fn nal(header: u8, len: usize) -> Vec<u8> {
        std::iter::once(header).chain((1..len).map(|i| (i % 251 + 1) as u8)).collect()
    }

    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        nals.iter().flat_map(|n| START_CODE.iter().chain(n)).copied().collect()
    }

    fn keyframe(idr_len: usize) -> Vec<u8> {
        annexb(&[vec![0x67, 0x42, 0xC0, 0x1F, 0x8C, 0x8D], vec![0x68, 0xCE, 0x3C, 0x80], nal(0x65, idr_len)])
    }

    fn delta(len: usize) -> Vec<u8> {
        annexb(&[nal(0x41, len)])
    }

    /// One sent RTP packet (owned).
    #[derive(Clone)]
    struct Sent {
        seq: u16,
        ts: u32,
        marker: bool,
        payload: Vec<u8>,
    }

    impl Sent {
        fn rtp(&self) -> RtpPacket<'_> {
            RtpPacket { ssrc: 7, seq: self.seq, timestamp: self.ts, marker: self.marker, payload: &self.payload }
        }
    }

    /// Packetize `frame` as the transport does: consecutive sequence numbers, marker on the last.
    fn send(frame: &[u8], seq: &mut u16, ts: u32) -> Vec<Sent> {
        let payloads = packetize(frame, MAX_PAYLOAD);
        let last = payloads.len() - 1;
        payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| {
                let s = Sent { seq: *seq, ts, marker: i == last, payload };
                *seq = seq.wrapping_add(1);
                s
            })
            .collect()
    }

    fn push_all(d: &mut Depacketizer, packets: &[Sent], now: Instant) {
        for p in packets {
            d.push(p.rtp(), now);
        }
    }

    #[test]
    fn nal_units_split_on_3_and_4_byte_start_codes() {
        let stream = [9, 9, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 0, 1, 0x65, 3, 0, 0, 1];
        let units: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(units, [&[0x67, 1][..], &[0x68, 2, 0], &[0x65, 3]], "junk before, one zero per 4-byte start code, empty tail");
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
        assert_eq!(nal_units(&[]).count(), 0);
    }

    #[test]
    fn nal_units_ending_in_zero_survive() {
        // DAVE ciphertext may end in 0 before the next 4-byte start code; single, STAP-A and FU-A.
        let mut big = nal(0x41, 3000);
        *big.last_mut().unwrap() = 0;
        let frame = annexb(&[vec![0x67, 0], vec![0x68, 7, 0], vec![0x41, 5, 0], big, nal(0x41, 9)]);
        let packets = packetize(&frame, MAX_PAYLOAD);
        assert!(packets.len() > 3 && packets[0][0] & 0x1F == STAP_A && packets[1][0] & 0x1F == FU_A);
        assert_eq!(assemble(packets.iter().map(|p| &p[..])), Some((frame, false)));
    }

    #[test]
    fn small_nal_units_are_aggregated_large_ones_fragmented() {
        let frame = keyframe(3000);
        let packets = packetize(&frame, MAX_PAYLOAD);
        // STAP-A [SPS, PPS], then the IDR in FU-A fragments.
        assert_eq!(packets[0], [&[0x78, 0, 6][..], &[0x67, 0x42, 0xC0, 0x1F, 0x8C, 0x8D], &[0, 4, 0x68, 0xCE, 0x3C, 0x80]].concat());
        assert_eq!(packets.len(), 1 + 2999_usize.div_ceil(MAX_PAYLOAD - 2));
        let fus = &packets[1..];
        assert!(fus.iter().all(|p| p.len() <= MAX_PAYLOAD && p[0] == 0x60 | FU_A));
        assert_eq!(fus[0][1], 0x80 | 5, "start");
        assert_eq!(fus[1][1], 5, "middle");
        assert_eq!(fus[2][1], 0x40 | 5, "end");
        assert_eq!(fus.iter().map(|p| p.len() - 2).sum::<usize>(), 2999);

        // SPS + PPS + a small slice in one STAP-A; a single small NAL unit goes whole.
        let small = annexb(&[vec![0x67, 1], vec![0x68, 2], nal(0x65, 100)]);
        assert_eq!(packetize(&small, MAX_PAYLOAD).len(), 1);
        assert_eq!(packetize(&delta(500), MAX_PAYLOAD), [nal(0x41, 500)]);
        assert!(packetize(&[], MAX_PAYLOAD).is_empty());
    }

    #[test]
    fn fu_a_boundaries() {
        // Exactly the payload size: one packet; one byte more: two fragments.
        assert_eq!(packetize(&delta(MAX_PAYLOAD), MAX_PAYLOAD), [nal(0x41, MAX_PAYLOAD)]);
        let two = packetize(&delta(MAX_PAYLOAD + 1), MAX_PAYLOAD);
        assert_eq!(two.len(), 2);
        assert_eq!((two[0].len(), two[1].len()), (MAX_PAYLOAD, 4));
        assert_eq!((two[0][1], two[1][1]), (0x81, 0x41));
        // The F bit and NRI stay in the indicator; the type moves to the header.
        let f = packetize(&annexb(&[nal(0xE5, 50)]), 10);
        assert!(f.iter().all(|p| p[0] == 0xE0 | FU_A && p[1] & 0x1F == 5 && p.len() <= 10));
        // A tiny payload size still makes progress.
        assert_eq!(packetize(&delta(5), 1).len(), 4);
        // Reassembled, they give the NAL unit back.
        for (len, max) in [(MAX_PAYLOAD + 1, MAX_PAYLOAD), (5000, MAX_PAYLOAD), (50, 10), (5, 3)] {
            let frame = delta(len);
            assert_eq!(assemble(packetize(&frame, max).iter().map(|p| &p[..])), Some((frame, false)), "{len} in {max}");
        }
    }

    #[test]
    fn malformed_payloads_are_refused() {
        let fu = |h: u8| vec![0x7C, h, 1, 2];
        let ok = |ps: &[Vec<u8>]| assemble(ps.iter().map(|p| &p[..])).map(|f| f.0);
        assert_eq!(ok(&[fu(0x85), fu(0x05), fu(0x45)]), Some(vec![0, 0, 0, 1, 0x65, 1, 2, 1, 2, 1, 2]));
        assert_eq!(ok(&[fu(0xC5)]), Some(vec![0, 0, 0, 1, 0x65, 1, 2]), "start and end in one FU is tolerated");
        assert_eq!(ok(&[fu(0x05), fu(0x45)]), None, "no start");
        assert_eq!(ok(&[fu(0x85), fu(0x85), fu(0x45)]), None, "start inside a fragmented unit");
        assert_eq!(ok(&[fu(0x85), fu(0x05)]), None, "no end");
        assert_eq!(ok(&[fu(0x85), vec![0x41, 1]]), None, "single NAL inside a fragmented unit");
        assert_eq!(ok(&[vec![0x7C, 0x85]]), None, "a start without an end");
        assert_eq!(ok(&[vec![0x7C]]), None, "short FU-A");
        assert_eq!(ok(&[vec![0x78, 0, 5, 0x41, 1]]), None, "STAP-A length beyond the packet");
        assert_eq!(ok(&[vec![0x78, 0, 0]]), None, "empty STAP-A unit");
        assert_eq!(ok(&[vec![0x78, 0, 1, 0x41, 9]]), None, "STAP-A trailing byte");
        assert_eq!(ok(&[vec![0x79, 0, 1, 0x41]]), None, "STAP-B is not supported");
        assert_eq!(ok(&[vec![0x7D, 0x85, 1]]), None, "FU-B is not supported");
        assert_eq!(ok(&[]), None);
    }

    #[test]
    fn frames_come_out_whole_and_in_order_across_wraparound() {
        let t0 = Instant::now();
        let mut seq = 65_530;
        let k = keyframe(3000);
        let (p1, p2) = (delta(500), delta(1500));
        let mut sent = send(&k, &mut seq, 1000);
        sent.extend(send(&p1, &mut seq, 4000));
        sent.extend(send(&p2, &mut seq, 7000));
        assert!(sent.iter().any(|s| s.seq == 65_535) && sent.iter().any(|s| s.seq == 0));

        // Reordered (each pair swapped) and duplicated.
        let mut d = Depacketizer::default();
        for pair in sent.chunks(2) {
            for s in pair.iter().rev() {
                d.push(s.rtp(), t0);
                d.push(s.rtp(), t0);
            }
        }
        assert_eq!(d.next_frame(), Some(k));
        assert_eq!(d.next_frame(), Some(p1));
        assert_eq!(d.next_frame(), Some(p2));
        assert_eq!(d.next_frame(), None);
        assert!(d.take_nacks(t0 + NACK_INTERVAL).is_empty());
        assert!(!d.take_keyframe_request(t0), "synced");
        assert!(d.packets.is_empty() && d.missing.is_empty());

        // A late copy of a taken packet is ignored.
        d.push(sent[0].rtp(), t0);
        assert!(d.packets.is_empty() && d.next_frame().is_none());
    }

    #[test]
    fn joining_mid_stream_waits_for_a_keyframe_and_asks_for_one() {
        let t0 = Instant::now();
        let mut d = Depacketizer::default();
        assert!(d.take_keyframe_request(t0), "nothing decodable yet");
        assert!(!d.take_keyframe_request(t0 + PLI_INTERVAL / 2));
        assert!(d.take_keyframe_request(t0 + PLI_INTERVAL));

        let mut seq = 100;
        let p0 = send(&delta(1500), &mut seq, 0);
        let p1 = send(&delta(300), &mut seq, 3000);
        let k = keyframe(2000);
        let kp = send(&k, &mut seq, 6000);
        let p2 = send(&delta(300), &mut seq, 9000);
        push_all(&mut d, &p0[1..], t0); // the tail of a frame
        push_all(&mut d, &p1, t0);
        assert_eq!(d.next_frame(), None, "a delta frame before any keyframe is dropped");
        push_all(&mut d, &kp, t0);
        push_all(&mut d, &p2, t0);
        assert_eq!(d.next_frame(), Some(k));
        assert_eq!(d.next_frame(), Some(delta(300)));
        assert!(!d.take_keyframe_request(t0 + PLI_INTERVAL * 4));

        // Joining at a keyframe whose predecessor never came: it opens with SPS.
        let mut d = Depacketizer::default();
        push_all(&mut d, &kp, t0);
        assert_eq!(d.next_frame(), Some(keyframe(2000)));
    }

    #[test]
    fn a_lost_packet_is_nacked_and_recovered_by_rtx() {
        let t0 = Instant::now();
        let mut d = Depacketizer::default();
        let mut seq = 65_534;
        push_all(&mut d, &send(&keyframe(500), &mut seq, 0), t0);
        assert!(d.next_frame().is_some());
        let p1 = send(&delta(3000), &mut seq, 3000);
        let p2 = send(&delta(200), &mut seq, 6000);
        let lost = p1[1].clone();
        push_all(&mut d, &[p1[0].clone(), p1[2].clone()], t0);
        push_all(&mut d, &p2, t0);
        assert_eq!(d.next_frame(), None, "waits for the gap");

        assert!(d.take_nacks(t0).is_empty(), "plain reordering gets a moment");
        assert_eq!(d.take_nacks(t0 + NACK_DELAY), [lost.seq]);
        assert!(d.take_nacks(t0 + NACK_DELAY + NACK_INTERVAL / 2).is_empty());
        assert_eq!(d.take_nacks(t0 + NACK_DELAY + NACK_INTERVAL), [lost.seq]);

        // The retransmission: RTX payload = original sequence number + original payload.
        let rtx = [&lost.seq.to_be_bytes()[..], &lost.payload].concat();
        let (osn, payload) = unwrap_rtx(&rtx).unwrap();
        d.push(RtpPacket { seq: osn, payload, ..lost.rtp() }, t0 + NACK_DELAY + NACK_INTERVAL);
        assert_eq!(d.next_frame(), Some(delta(3000)));
        assert_eq!(d.next_frame(), Some(delta(200)));
        assert!(d.take_nacks(t0 + Duration::from_secs(1)).is_empty());
        assert!(!d.take_keyframe_request(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn a_packet_lost_for_good_drops_its_frame_until_the_next_keyframe() {
        let t0 = Instant::now();
        let mut d = Depacketizer::default();
        let mut seq = 1;
        push_all(&mut d, &send(&keyframe(500), &mut seq, 0), t0);
        assert!(d.next_frame().is_some());
        d.take_keyframe_request(t0);
        let p1 = send(&delta(3000), &mut seq, 3000);
        let lost = p1[1].seq;
        push_all(&mut d, &[p1[0].clone(), p1[2].clone()], t0);
        push_all(&mut d, &send(&delta(200), &mut seq, 6000), t0);

        let nacks: Vec<Vec<u16>> = (0..6).map(|i| d.take_nacks(t0 + NACK_DELAY + NACK_INTERVAL * i)).collect();
        assert_eq!(nacks.iter().filter(|n| *n == &[lost]).count(), usize::from(NACK_TRIES), "{nacks:?}");
        assert!(nacks.iter().all(|n| n.is_empty() || n == &[lost]));
        assert_eq!(d.next_frame(), None, "the frame with the hole and the delta after it are dropped");
        let at = t0 + LOSS_TIMEOUT;
        assert!(d.take_keyframe_request(at));
        assert!(!d.take_keyframe_request(at + PLI_INTERVAL / 2), "rate limited");

        push_all(&mut d, &send(&delta(200), &mut seq, 9000), at);
        assert_eq!(d.next_frame(), None);
        push_all(&mut d, &send(&keyframe(800), &mut seq, 12_000), at);
        assert_eq!(d.next_frame(), Some(keyframe(800)));
        assert!(!d.take_keyframe_request(at + PLI_INTERVAL * 2));

        // A late retransmission of the given-up packet changes nothing.
        d.push(RtpPacket { seq: lost, ..p1[1].rtp() }, at);
        assert_eq!(d.next_frame(), None);

        // The decoder failing does the same.
        d.wait_for_keyframe();
        push_all(&mut d, &send(&delta(200), &mut seq, 15_000), at);
        assert_eq!(d.next_frame(), None);
        assert!(d.take_keyframe_request(at + PLI_INTERVAL * 2));
    }

    #[test]
    fn a_packet_lost_early_in_a_big_keyframe_is_recovered_by_rtx() {
        // 400 packets (a 1080p screen-share IDR): the gap stays NACKable and its late RTX counts.
        let t0 = Instant::now();
        let mut d = Depacketizer::default();
        let mut seq = 65_000;
        let k = keyframe(399 * (MAX_PAYLOAD - 2));
        let sent = send(&k, &mut seq, 0);
        assert_eq!(sent.len(), 400);
        let lost = sent[5].clone();
        push_all(&mut d, &sent[..5], t0);
        push_all(&mut d, &sent[6..], t0);
        assert_eq!(d.next_frame(), None, "waits for the gap");
        assert_eq!(d.take_nacks(t0 + NACK_DELAY), [lost.seq]);
        let rtx = [&lost.seq.to_be_bytes()[..], &lost.payload].concat();
        let (osn, payload) = unwrap_rtx(&rtx).unwrap();
        d.push(RtpPacket { seq: osn, payload, ..lost.rtp() }, t0 + NACK_DELAY);
        assert_eq!(d.next_frame(), Some(k));
        assert!(!d.take_keyframe_request(t0 + NACK_DELAY));
    }

    #[test]
    fn a_frame_without_marker_ends_at_the_next_timestamp() {
        let t0 = Instant::now();
        let mut d = Depacketizer::default();
        let mut seq = 10;
        let mut k = send(&keyframe(300), &mut seq, 0);
        k.last_mut().unwrap().marker = false;
        push_all(&mut d, &k, t0);
        assert_eq!(d.next_frame(), None);
        push_all(&mut d, &send(&delta(100), &mut seq, 3000), t0);
        assert_eq!(d.next_frame(), Some(keyframe(300)));
        assert_eq!(d.next_frame(), Some(delta(100)));
    }

    #[test]
    fn memory_stays_bounded() {
        let t0 = Instant::now();
        let mut d = Depacketizer::default();
        // A frame that never completes (its first packet never comes, no NACK rounds run).
        d.push(RtpPacket { ssrc: 1, seq: 0, timestamp: 0, marker: true, payload: &[0x41] }, t0);
        for i in 2..6000u16 {
            d.push(RtpPacket { ssrc: 1, seq: i, timestamp: 9, marker: false, payload: &[0x7C, 0x05, 1] }, t0);
            assert!(d.packets.len() <= REORDER_PACKETS + MAX_FRAME_PACKETS);
            assert!(d.missing.len() <= REORDER_PACKETS + MAX_FRAME_PACKETS);
        }
        assert!(d.next_frame().is_none());
        // A big jump starts over.
        d.push(RtpPacket { ssrc: 1, seq: 30_000, timestamp: 1, marker: true, payload: &[0x41] }, t0);
        assert_eq!((d.packets.len(), d.missing.len()), (1, 0));
        // Empty payloads (padding) are ignored.
        d.push(RtpPacket { ssrc: 1, seq: 30_001, timestamp: 1, marker: true, payload: &[] }, t0);
        assert_eq!(d.packets.len(), 1);
    }

    #[test]
    fn rtx_payloads_unwrap() {
        assert_eq!(unwrap_rtx(&[0x12, 0x34, 0x41, 7]), Some((0x1234, &[0x41, 7][..])));
        assert_eq!(unwrap_rtx(&[0x12, 0x34]), None, "padding only");
        assert_eq!(unwrap_rtx(&[0x12]), None);
    }

    #[test]
    fn rtx_history_keeps_one_second() {
        let t0 = Instant::now();
        let mut h = RtxHistory::default();
        assert_eq!(h.get(1), None);
        for (i, seq) in [65_534u16, 65_535, 0, 1].into_iter().enumerate() {
            h.keep(seq, &[i as u8; 3], t0);
        }
        assert_eq!(h.get(65_535), Some(&[1u8; 3][..]));
        assert_eq!(h.get(1), Some(&[3u8; 3][..]));
        assert_eq!(h.get(2), None);
        h.keep(2, &[9], t0 + RTX_HISTORY + Duration::from_millis(1));
        assert_eq!((h.get(65_534), h.get(1), h.get(2)), (None, None, Some(&[9u8][..])), "older than 1 s");
        // Not consecutive: still found.
        h.keep(10, &[10], t0 + RTX_HISTORY * 2);
        h.keep(5, &[5], t0 + RTX_HISTORY * 2);
        assert_eq!((h.get(10), h.get(5)), (Some(&[10u8][..]), Some(&[5u8][..])));
        for i in 0..3000u16 {
            h.keep(i, &[0; 1100], t0 + RTX_HISTORY * 2);
        }
        assert_eq!(h.sent.len(), RTX_PACKETS);
        assert_eq!(h.get(2999).map(<[u8]>::len), Some(1100));
    }

    #[test]
    fn rtcp_feedback_round_trips() {
        let n = nack(0x0102_0304, 0xAABB_CCDD, &[65_534, 65_535, 0, 5, 40, 40]);
        assert_eq!(
            n,
            [0x81, 205, 0, 4, 1, 2, 3, 4, 0xAA, 0xBB, 0xCC, 0xDD, 0xFF, 0xFE, 0b0000_0000, 0b0100_0011, 0, 40, 0, 0]
        );
        let p = pli(1, 0xAABB_CCDD);
        assert_eq!(p, [0x81, 206, 0, 2, 0, 0, 0, 1, 0xAA, 0xBB, 0xCC, 0xDD]);
        assert!(nack(1, 2, &[]).is_empty());

        // Compound: a receiver report first, then the feedback.
        let rr = [0x80, 201, 0, 1, 0, 0, 0, 9];
        let compound = [&rr[..], &n, &p].concat();
        assert_eq!(
            parse_feedback(&compound),
            [Feedback::Nack { media_ssrc: 0xAABB_CCDD, lost: vec![65_534, 65_535, 0, 5, 40] }, Feedback::Pli { media_ssrc: 0xAABB_CCDD }]
        );
        // Padding, truncation, garbage, other feedback types.
        let mut padded = pli(1, 2);
        padded[0] |= 0x20;
        padded[3] = 3;
        padded.extend_from_slice(&[0, 0, 0, 4]);
        assert_eq!(parse_feedback(&padded), [Feedback::Pli { media_ssrc: 2 }]);
        assert_eq!(parse_feedback(&compound[..compound.len() - 1]), [Feedback::Nack { media_ssrc: 0xAABB_CCDD, lost: vec![65_534, 65_535, 0, 5, 40] }]);
        assert!(parse_feedback(&[0x81, 206, 0, 0]).is_empty(), "too short for a PLI");
        assert!(parse_feedback(&[0x41, 206, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2]).is_empty(), "not version 2");
        assert!(parse_feedback(&[0x8F, 206, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2]).is_empty(), "FMT 15 (application layer)");
        assert!(parse_feedback(&[]).is_empty());
    }

    #[test]
    fn video_header_has_the_extensions() {
        let mut out = vec![0xFF; 3];
        let clear = write_video_header(&mut out, 103, 0x1234, 0x0102_0304, 0xAABB_CCDD, true, false);
        assert_eq!(clear, 16);
        assert_eq!(
            out,
            [0x90, 0x80 | 103, 0x12, 0x34, 1, 2, 3, 4, 0xAA, 0xBB, 0xCC, 0xDD, 0xBE, 0xDE, 0, 2, 0x62, 0, 0, 0, 0xB2, b'1', b'0', b'0']
        );
        let clear = write_video_header(&mut out, 104, 1, 2, 3, false, true);
        assert_eq!(clear, 16);
        assert_eq!(out.len(), 28);
        assert_eq!((out[1], &out[14..16], &out[24..]), (104, &[0, 3][..], &[0x70, 1, 0, 0][..]));
    }

    /// Camera picture → encoder → packets (reordered, one lost and retransmitted) → depacketizer
    /// → decoder → the UI's frame slot.
    #[test]
    fn encode_packetize_depacketize_decode_round_trip() {
        use crate::model::Id;
        use crate::voice::video::codec::{Decoder, Encoder, I420, to_i420};
        use crate::voice::video::{PixelFormat, RawFrame, SendLimits, VideoFrames, VideoKey};

        let (w, h) = (640u32, 360u32);
        // 16×16 blocks of assorted colours (R, G, B).
        let colour = |x: u32, y: u32| {
            let b = (x / 16) * 7 + (y / 16) * 13;
            [(b * 37 % 200 + 28) as u8, (b * 91 % 200 + 28) as u8, (b * 53 % 200 + 28) as u8]
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let [r, g, b] = colour(i as u32 % w, i as u32 / w);
            *px = [b, g, r, 255];
        }
        let mut pic = I420::new(w, h);
        to_i420(&RawFrame { width: w, height: h, stride: w as usize * 4, format: PixelFormat::Bgra, data: &bgra }, &mut pic);
        let mut enc = Encoder::new(w, h, SendLimits::CAMERA, false).unwrap();
        let t0 = Instant::now();

        let mut seq = 65_500;
        let mut sent = Vec::new();
        let mut frames = 0;
        for n in 0..5u32 {
            let captured = t0 + Duration::from_millis(33 * u64::from(n));
            if let Some(f) = enc.encode(&pic, false, captured).unwrap() {
                assert_eq!(f.keyframe, frames == 0);
                frames += 1;
                sent.extend(send(&f.annexb, &mut seq, n * 3000));
            }
        }
        assert!(sent.len() > 3, "the keyframe needs several packets");

        let mut d = Depacketizer::default();
        let lost = sent.remove(1);
        for pair in sent.chunks(2) {
            for s in pair.iter().rev() {
                d.push(s.rtp(), t0);
            }
        }
        assert_eq!(d.next_frame(), None);
        assert_eq!(d.take_nacks(t0 + NACK_DELAY), [lost.seq]);
        d.push(lost.rtp(), t0 + NACK_DELAY);

        let slots = VideoFrames::default();
        let key = VideoKey::User(Id(3));
        let writer = slots.writer(key.clone()).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut decoded = 0;
        while let Some(frame) = d.next_frame() {
            assert_eq!(dec.decode(&frame, &writer).unwrap(), Some((w, h)));
            decoded += 1;
        }
        assert_eq!(decoded, frames);
        let (generation, image) = slots.latest(&key).unwrap();
        assert_eq!((generation, image.size), (frames as u64, [w as usize, h as usize]));
        for (x, y) in [(8, 8), (328, 168), (632, 344), (200, 40)] {
            let (px, [r, g, b]) = (image.pixels[(y * w + x) as usize], colour(x, y));
            assert!(px.r().abs_diff(r) < 20 && px.g().abs_diff(g) < 20 && px.b().abs_diff(b) < 20, "({x},{y}): {px:?} vs {r},{g},{b}");
        }
    }
}

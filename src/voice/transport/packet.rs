//! Pure packet code of the voice transport: IP discovery, RTP headers and the rtpsize AEAD
//! transport encryption (VOICE_NOTES §2.6–2.8). No I/O.

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{AeadInPlace, Error, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;

/// Opus payload type (the only codec declared in Select Protocol).
pub const OPUS_PT: u8 = 120;
/// Fixed RTP header length.
pub const RTP_HEADER: usize = 12;
const TAG: usize = 16;
/// The big-endian nonce counter appended after the tag.
const NONCE_SUFFIX: usize = 4;

/// Transport encryption mode (§2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `aead_aes256_gcm_rtpsize`: 12-byte nonce. Preferred when Ready offers it.
    Aes256Gcm,
    /// `aead_xchacha20_poly1305_rtpsize`: 24-byte nonce. Always offered.
    XChaCha20,
}

impl Mode {
    /// In order of preference.
    const ALL: [Mode; 2] = [Mode::Aes256Gcm, Mode::XChaCha20];

    /// Pick from Ready `modes`: AES-GCM if offered, else XChaCha20, else None.
    pub fn pick(modes: &[String]) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| modes.iter().any(|s| s == m.wire_name()))
    }

    /// The mode a Session Description names.
    pub fn from_wire(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.wire_name() == name)
    }

    /// Select Protocol `mode` string.
    pub fn wire_name(self) -> &'static str {
        match self {
            Mode::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            Mode::XChaCha20 => "aead_xchacha20_poly1305_rtpsize",
        }
    }
}

/// 74-byte IP discovery request for `ssrc` (§2.6).
pub fn discovery_request(ssrc: u32) -> [u8; 74] {
    let mut req = [0u8; 74];
    req[0..2].copy_from_slice(&1u16.to_be_bytes()); // type: request
    req[2..4].copy_from_slice(&70u16.to_be_bytes()); // length after type + length
    req[4..8].copy_from_slice(&ssrc.to_be_bytes());
    req
}

/// Parse an IP discovery response into `(ip, port)`; None if it isn't one.
pub fn parse_discovery(resp: &[u8]) -> Option<(String, u16)> {
    if resp.len() != 74 || resp[0..2] != [0, 2] {
        return None;
    }
    let ip = &resp[8..72];
    let ip = &ip[..ip.iter().position(|&b| b == 0).unwrap_or(ip.len())];
    // Validated: it goes back to the server in Select Protocol.
    let ip: std::net::IpAddr = std::str::from_utf8(ip).ok()?.parse().ok()?;
    Some((ip.to_string(), u16::from_be_bytes([resp[72], resp[73]])))
}

/// Received RTP header fields (§2.7, §2.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Bytes that stay in clear: 12 + 4·CC, + 4 for the 0xBEDE preamble when X is set.
    pub clear_len: usize,
    /// Extension length in 32-bit words (its elements are encrypted; skip them after decrypt).
    pub ext_words: u16,
    /// P bit: the last decrypted byte counts the padding bytes at the end.
    pub padding: bool,
}

impl RtpHeader {
    /// The media payload inside the decrypted part: without extension elements and padding.
    pub fn payload<'a>(&self, plain: &'a [u8]) -> Option<&'a [u8]> {
        let body = plain.get(usize::from(self.ext_words) * 4..)?;
        let pad = if self.padding { usize::from(*body.last()?) } else { 0 };
        body.get(..body.len().checked_sub(pad)?)
    }
}

/// Parse the clear part of an RTP packet; None for RTCP, short or non-v2 packets.
pub fn parse_rtp(pkt: &[u8]) -> Option<RtpHeader> {
    // RTCP: PT 192..=223 sits where RTP has marker + payload type (§2.6 demux).
    if pkt.len() < RTP_HEADER || pkt[0] >> 6 != 2 || (192..=223).contains(&pkt[1]) {
        return None;
    }
    let mut clear_len = RTP_HEADER + 4 * usize::from(pkt[0] & 0x0F);
    let mut ext_words = 0;
    if pkt[0] & 0x10 != 0 {
        let preamble = pkt.get(clear_len..clear_len + 4)?;
        ext_words = u16::from_be_bytes([preamble[2], preamble[3]]);
        clear_len += 4;
    }
    if pkt.len() < clear_len {
        return None;
    }
    let be32 = |i: usize| u32::from_be_bytes([pkt[i], pkt[i + 1], pkt[i + 2], pkt[i + 3]]);
    Some(RtpHeader {
        payload_type: pkt[1] & 0x7F,
        seq: u16::from_be_bytes([pkt[2], pkt[3]]),
        timestamp: be32(4),
        ssrc: be32(8),
        clear_len,
        ext_words,
        padding: pkt[0] & 0x20 != 0,
    })
}

/// Append the 12-byte header of an outgoing Opus packet (V2, PT 120, no extension) to `out`.
pub fn write_rtp_header(out: &mut Vec<u8>, seq: u16, timestamp: u32, ssrc: u32) {
    out.extend_from_slice(&[0x80, OPUS_PT]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
}

/// Select Protocol `rtc_connection_id`: a random UUID v4 from 16 random bytes.
pub fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = b[6] & 0x0F | 0x40; // version 4
    b[8] = b[8] & 0x3F | 0x80; // RFC 4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

enum Cipher {
    Aes(Box<Aes256Gcm>),
    XChaCha(XChaCha20Poly1305),
}

/// Transport crypto of one connection: `secret_key` + the shared outgoing nonce counter
/// (random start, `wrapping_add(1)` per packet). No Debug: it holds the key.
pub struct Crypt {
    cipher: Cipher,
    counter: u32,
}

impl Crypt {
    pub fn new(mode: Mode, secret_key: &[u8; 32]) -> Crypt {
        Crypt::with_counter(mode, secret_key, getrandom::u32().unwrap_or(0))
    }

    fn with_counter(mode: Mode, key: &[u8; 32], counter: u32) -> Crypt {
        let key = GenericArray::from_slice(key);
        let cipher = match mode {
            Mode::Aes256Gcm => Cipher::Aes(Box::new(Aes256Gcm::new(key))),
            Mode::XChaCha20 => Cipher::XChaCha(XChaCha20Poly1305::new(key)),
        };
        Crypt { cipher, counter }
    }

    /// Encrypt `pkt[clear_len..]` in place (AAD = `pkt[..clear_len]`), then append the 16-byte
    /// tag and the 4-byte big-endian nonce counter.
    pub fn seal(&mut self, pkt: &mut Vec<u8>, clear_len: usize) -> Result<(), Error> {
        let suffix = self.counter.to_be_bytes();
        let (aad, body) = pkt.split_at_mut(clear_len);
        let tag = match &self.cipher {
            Cipher::Aes(c) => c.encrypt_in_place_detached(GenericArray::from_slice(&nonce::<12>(suffix)), aad, body)?,
            Cipher::XChaCha(c) => c.encrypt_in_place_detached(GenericArray::from_slice(&nonce::<24>(suffix)), aad, body)?,
        };
        pkt.extend_from_slice(&tag);
        pkt.extend_from_slice(&suffix);
        self.counter = self.counter.wrapping_add(1);
        Ok(())
    }

    /// Decrypt a received packet in place; returns the plaintext after `clear_len` (still
    /// starting with the extension elements). None if it is too short or the tag doesn't verify.
    pub fn open<'a>(&self, pkt: &'a mut [u8], clear_len: usize) -> Option<&'a [u8]> {
        let body_end = pkt.len().checked_sub(TAG + NONCE_SUFFIX).filter(|&end| end >= clear_len)?;
        let (head, trailer) = pkt.split_at_mut(body_end);
        let tag = GenericArray::from_slice(&trailer[..TAG]);
        let suffix = [trailer[TAG], trailer[TAG + 1], trailer[TAG + 2], trailer[TAG + 3]];
        let (aad, body) = head.split_at_mut(clear_len);
        let ok = match &self.cipher {
            Cipher::Aes(c) => c.decrypt_in_place_detached(GenericArray::from_slice(&nonce::<12>(suffix)), aad, body, tag),
            Cipher::XChaCha(c) => c.decrypt_in_place_detached(GenericArray::from_slice(&nonce::<24>(suffix)), aad, body, tag),
        };
        ok.ok().map(|()| &*body)
    }
}

/// rtpsize nonce: the 4-byte counter first, the rest zero.
fn nonce<const N: usize>(suffix: [u8; 4]) -> [u8; N] {
    let mut n = [0u8; N];
    n[..4].copy_from_slice(&suffix);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        let offered = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let both = offered(&["aead_xchacha20_poly1305_rtpsize", "aead_aes256_gcm_rtpsize", "xsalsa20_poly1305"]);
        assert_eq!(Mode::pick(&both), Some(Mode::Aes256Gcm));
        assert_eq!(Mode::pick(&offered(&["aead_xchacha20_poly1305_rtpsize"])), Some(Mode::XChaCha20));
        assert_eq!(Mode::pick(&offered(&["xsalsa20_poly1305", "aead_aes256_gcm"])), None);
        for m in Mode::ALL {
            assert_eq!(Mode::from_wire(m.wire_name()), Some(m));
        }
        assert_eq!(Mode::from_wire("aead_aes256_gcm"), None);
    }

    #[test]
    fn ip_discovery() {
        let req = discovery_request(0x0102_0304);
        assert_eq!(req[..8], [0, 1, 0, 70, 1, 2, 3, 4]);
        assert!(req[8..].iter().all(|&b| b == 0));

        let mut resp = [0u8; 74];
        resp[..8].copy_from_slice(&[0, 2, 0, 70, 1, 2, 3, 4]);
        resp[8..8 + 11].copy_from_slice(b"203.0.113.7");
        resp[72..].copy_from_slice(&50_004u16.to_be_bytes());
        assert_eq!(parse_discovery(&resp), Some(("203.0.113.7".into(), 50_004)));

        assert_eq!(parse_discovery(&req), None, "a request is not a response");
        assert_eq!(parse_discovery(&resp[..73]), None);
        resp[8..8 + 11].copy_from_slice(b"not-an-ip.x");
        assert_eq!(parse_discovery(&resp), None);
    }

    #[test]
    fn rtp_round_trip() {
        let mut pkt = Vec::new();
        write_rtp_header(&mut pkt, 65_535, 0xDEAD_BEEF, 42);
        assert_eq!(pkt.len(), RTP_HEADER);
        assert_eq!(pkt[..2], [0x80, 0x78]);
        pkt.extend_from_slice(b"opus");
        let h = parse_rtp(&pkt).unwrap();
        let want = RtpHeader {
            payload_type: OPUS_PT,
            seq: 65_535,
            timestamp: 0xDEAD_BEEF,
            ssrc: 42,
            clear_len: RTP_HEADER,
            ext_words: 0,
            padding: false,
        };
        assert_eq!(h, want);
        assert_eq!(h.payload(&pkt[h.clear_len..]), Some(&b"opus"[..]));
    }

    #[test]
    fn rtp_csrc_extension_padding_and_rtcp() {
        // V2, P, X, CC=1; marker + PT 120; one CSRC; 0xBEDE preamble with 1 word.
        let mut pkt = vec![0xB1, 0x80 | OPUS_PT, 0, 7, 0, 0, 0, 9, 0, 0, 0, 5];
        pkt.extend_from_slice(&[0, 0, 0, 1]); // CSRC
        pkt.extend_from_slice(&[0xBE, 0xDE, 0, 1]);
        let h = parse_rtp(&pkt).unwrap();
        assert_eq!((h.payload_type, h.seq, h.timestamp, h.ssrc), (OPUS_PT, 7, 9, 5));
        assert_eq!((h.clear_len, h.ext_words, h.padding), (20, 1, true));
        // Decrypted part: 4 bytes of extension elements, payload, 2 padding bytes.
        let plain = [0x10, 0xFF, 0, 0, b'o', b'k', 0, 2];
        assert_eq!(h.payload(&plain), Some(&b"ok"[..]));
        assert_eq!(h.payload(&[0x10, 0, 0]), None, "shorter than the extension");
        assert_eq!(h.payload(&[0, 0, 0, 0, 9]), None, "more padding than payload");

        assert!(parse_rtp(&pkt[..18]).is_none(), "extension preamble cut off");
        assert!(parse_rtp(&[0x80, 200, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0]).is_none(), "RTCP sender report");
        assert!(parse_rtp(&[0x13, 0x37, 0xF0, 0x0D, 0, 0, 0, 1, 0, 0, 0, 0]).is_none(), "ping reply");
        assert!(parse_rtp(&pkt[..11]).is_none());
    }

    #[test]
    fn seal_matches_the_wire_layout_and_opens() {
        let key = [7u8; 32];
        let mut hdr = Vec::new();
        write_rtp_header(&mut hdr, 1, 2, 3);
        for mode in Mode::ALL {
            let mut c = Crypt::with_counter(mode, &key, 0x0102_0304);
            let mut pkt = hdr.clone();
            pkt.extend_from_slice(b"frame");
            c.seal(&mut pkt, RTP_HEADER).unwrap();
            assert_eq!(pkt.len(), RTP_HEADER + 5 + TAG + NONCE_SUFFIX);
            assert_eq!(pkt[..RTP_HEADER], hdr[..], "header stays in clear");
            assert_eq!(pkt[pkt.len() - 4..], [1, 2, 3, 4], "big-endian counter suffix");

            // Independent check: nonce = counter in the first 4 bytes, AAD = RTP header.
            let mut expect = b"frame".to_vec();
            let tag = match mode {
                Mode::Aes256Gcm => Aes256Gcm::new(&key.into())
                    .encrypt_in_place_detached(&nonce::<12>([1, 2, 3, 4]).into(), &hdr, &mut expect),
                Mode::XChaCha20 => XChaCha20Poly1305::new(&key.into())
                    .encrypt_in_place_detached(&nonce::<24>([1, 2, 3, 4]).into(), &hdr, &mut expect),
            }
            .unwrap();
            expect.extend_from_slice(&tag);
            assert_eq!(pkt[RTP_HEADER..pkt.len() - 4], expect[..], "{mode:?}");

            let mut rx = pkt.clone();
            assert_eq!(c.open(&mut rx, RTP_HEADER), Some(&b"frame"[..]));
            let mut tampered = pkt.clone();
            tampered[1] ^= 1; // the header is authenticated
            assert_eq!(c.open(&mut tampered, RTP_HEADER), None);
            assert_eq!(Crypt::with_counter(mode, &[8; 32], 0).open(&mut pkt.clone(), RTP_HEADER), None, "wrong key");
            assert_eq!(c.open(&mut [0u8; 19], 12), None, "too short");
        }
    }

    #[test]
    fn nonce_counter_increments_and_wraps() {
        let mut c = Crypt::with_counter(Mode::XChaCha20, &[1; 32], u32::MAX);
        let mut suffixes = Vec::new();
        for _ in 0..2 {
            let mut pkt = vec![0x80, OPUS_PT, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            c.seal(&mut pkt, RTP_HEADER).unwrap();
            suffixes.push(pkt[pkt.len() - 4..].to_vec());
        }
        assert_eq!(suffixes, [vec![0xFF; 4], vec![0; 4]]);
    }

    #[test]
    fn uuid_is_v4() {
        let id = uuid_v4([0xFF; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
    }
}

//! The app inside the setup exe: `<setup stub><deflate(rustcord.exe)><trailer>`, the trailer
//! being the compressed length (u64 LE), the app's length (u64 LE) and `MAGIC`. `--pack`
//! appends it after linking (scripts/build-installer.ps1), so `cargo build` needs no payload.

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use std::io::{self, Read, Write};

const MAGIC: &[u8; 8] = b"RCSETUP1";
const TRAILER: usize = 8 + 8 + MAGIC.len();

/// The compressed app and its unpacked size.
pub struct Payload<'a> {
    pub data: &'a [u8],
    pub len: u64,
}

/// Splits a setup exe into its stub (the setup program itself) and the payload, if any.
pub fn split(exe: &[u8]) -> (&[u8], Option<Payload<'_>>) {
    let Some(body_len) = exe.len().checked_sub(TRAILER).filter(|_| exe.ends_with(MAGIC)) else { return (exe, None) };
    let field = |at: usize| u64::from_le_bytes(exe[body_len + at..body_len + at + 8].try_into().unwrap_or_default());
    match usize::try_from(field(0)).ok().and_then(|n| body_len.checked_sub(n)) {
        Some(start) => (&exe[..start], Some(Payload { data: &exe[start..body_len], len: field(8) })),
        None => (exe, None),
    }
}

/// `stub` followed by `app`, compressed, and the trailer.
pub fn pack(stub: &[u8], app: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = DeflateEncoder::new(stub.to_vec(), Compression::best());
    enc.write_all(app)?;
    let mut out = enc.finish()?;
    let compressed = (out.len() - stub.len()) as u64;
    out.extend_from_slice(&compressed.to_le_bytes());
    out.extend_from_slice(&(app.len() as u64).to_le_bytes());
    out.extend_from_slice(MAGIC);
    Ok(out)
}

/// Inflates the payload into `dst`, reporting the share done (0–1) after each chunk.
pub fn unpack(p: &Payload<'_>, dst: &mut impl Write, mut progress: impl FnMut(f32)) -> io::Result<()> {
    let mut dec = DeflateDecoder::new(p.data);
    let mut buf = vec![0; 1 << 20];
    let mut done = 0u64;
    loop {
        let n = dec.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        done += n as u64;
        progress(done as f32 / p.len.max(1) as f32);
    }
    if done != p.len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the setup file is damaged; download it again"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips() {
        let stub = b"MZ setup stub".to_vec();
        // Some structure and some noise, like an exe.
        let app: Vec<u8> = (0..300_000u32).map(|i| if i % 7 == 0 { (i.wrapping_mul(2654435761) >> 13) as u8 } else { (i / 1000) as u8 }).collect();
        let packed = pack(&stub, &app).unwrap();
        assert!(packed.len() < stub.len() + app.len());

        let (s, p) = split(&packed);
        assert_eq!(s, &stub[..]);
        let p = p.expect("payload");
        assert_eq!(p.len, app.len() as u64);
        let (mut out, mut last) = (Vec::new(), 0.0);
        unpack(&p, &mut out, |f| last = f).unwrap();
        assert_eq!(out, app);
        assert_eq!(last, 1.0);

        // Packing a packed setup again (its stub is used) replaces the payload.
        let again = pack(split(&packed).0, b"v2").unwrap();
        let (s, p) = split(&again);
        assert_eq!(s, &stub[..]);
        let mut out = Vec::new();
        unpack(&p.unwrap(), &mut out, |_| {}).unwrap();
        assert_eq!(out, b"v2");
    }

    #[test]
    fn no_or_broken_payload() {
        assert!(split(b"MZ plain cargo build").1.is_none());
        assert!(split(b"").1.is_none());
        // A trailer whose length points before the start is no payload.
        let mut bad = b"MZ".to_vec();
        bad.extend_from_slice(&u64::MAX.to_le_bytes());
        bad.extend_from_slice(&5u64.to_le_bytes());
        bad.extend_from_slice(MAGIC);
        assert!(split(&bad).1.is_none());
        // A truncated download fails instead of installing half an exe.
        let packed = pack(b"MZ", &[7u8; 50_000]).unwrap();
        let (_, p) = split(&packed);
        let p = p.unwrap();
        let cut = Payload { data: &p.data[..p.data.len() / 2], len: p.len };
        assert!(unpack(&cut, &mut Vec::new(), |_| {}).is_err());
    }
}

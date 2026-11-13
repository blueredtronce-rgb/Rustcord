//! `compress=zlib-stream` transport decompression: one zlib context per WebSocket
//! connection; a payload is complete when the buffered frames end with the sync-flush marker.

use flate2::{Decompress, FlushDecompress, Status};
use std::io;

const SYNC_FLUSH: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

pub(super) struct ZlibStream {
    z: Decompress,
    buf: Vec<u8>,
}

impl ZlibStream {
    pub fn new() -> Self {
        Self { z: Decompress::new(true), buf: Vec::new() }
    }

    /// Feeds one binary frame. Returns the decompressed JSON once a whole payload arrived.
    pub fn push(&mut self, frame: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.buf.extend_from_slice(frame);
        if !self.buf.ends_with(&SYNC_FLUSH) {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(self.buf.len() * 4 + 1024);
        let mut pos = 0;
        loop {
            if out.capacity() - out.len() < 8 * 1024 {
                out.reserve(out.capacity().max(64 * 1024));
            }
            let (in0, out0) = (self.z.total_in(), self.z.total_out());
            let status = self.z.decompress_vec(&self.buf[pos..], &mut out, FlushDecompress::Sync)?;
            pos += (self.z.total_in() - in0) as usize;
            // All input consumed and spare output room left => nothing is pending in the decoder.
            if status == Status::StreamEnd || (pos == self.buf.len() && out.len() < out.capacity()) {
                break;
            }
            if self.z.total_in() == in0 && self.z.total_out() == out0 {
                return Err(io::Error::other("zlib stream stalled"));
            }
        }
        self.buf.clear();
        // READY can be megabytes; don't keep that capacity for the connection's lifetime.
        self.buf.shrink_to(64 * 1024);
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    fn compress(c: &mut Compress, msg: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(msg.len() + 1024);
        let before = c.total_in();
        c.compress_vec(msg, &mut out, FlushCompress::Sync).unwrap();
        assert_eq!((c.total_in() - before) as usize, msg.len());
        assert!(out.ends_with(&SYNC_FLUSH));
        out
    }

    #[test]
    fn inflates_split_frames_and_large_payloads() {
        let mut c = Compress::new(Compression::default(), true);
        let hello = br#"{"op":10,"d":{"heartbeat_interval":41250},"s":null,"t":null}"#;
        let big: Vec<u8> = {
            let mut s = String::from(r#"{"op":0,"t":"READY","d":{"x":""#);
            for i in 0..40_000 {
                s.push_str(&format!("{i},"));
            }
            s.push_str(r#""}}"#);
            s.into_bytes()
        };

        let first = compress(&mut c, hello);
        let second = compress(&mut c, &big);

        let mut z = ZlibStream::new();
        let (a, b) = first.split_at(first.len() / 2);
        assert!(z.push(a).unwrap().is_none());
        assert_eq!(z.push(b).unwrap().unwrap(), hello);

        // Second payload split in three, shares the same zlib context.
        let n = second.len() / 3;
        assert!(z.push(&second[..n]).unwrap().is_none());
        assert!(z.push(&second[n..2 * n]).unwrap().is_none());
        assert_eq!(z.push(&second[2 * n..]).unwrap().unwrap(), big);

        // And a small one after the big one.
        let third = compress(&mut c, b"{\"op\":11}");
        assert_eq!(z.push(&third).unwrap().unwrap(), b"{\"op\":11}");
    }
}

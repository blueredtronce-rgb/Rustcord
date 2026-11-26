//! NVIDIA's installed H.264 encoder, fed a reusable DX11 NV12 texture.

use std::ffi::{c_char, c_int, c_void};
use std::ptr::{self, NonNull};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

use crate::voice::video::{EncodedFrame, SendLimits};

type Result<T> = std::result::Result<T, String>;

#[repr(C)]
struct Packet {
    data: *const u8,
    length: u32,
    keyframe: u32,
}

unsafe extern "C" {
    fn rc_nvenc_open(
        device: *mut c_void,
        texture: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        error: *mut c_char,
        capacity: u32,
    ) -> *mut c_void;
    fn rc_nvenc_encode(
        session: *mut c_void,
        timestamp: u64,
        keyframe: c_int,
        packet: *mut Packet,
        error: *mut c_char,
        capacity: u32,
    ) -> c_int;
    fn rc_nvenc_release(session: *mut c_void, error: *mut c_char, capacity: u32) -> c_int;
    fn rc_nvenc_close(session: *mut c_void);
}

pub(super) struct Encoder {
    session: NonNull<c_void>,
    start: Option<Instant>,
    _texture: ID3D11Texture2D,
    _device: ID3D11Device,
}

// The driver handles are owned by this encoder. Capture transfers the owner to
// its worker; the driver is called from one thread at a time through &mut self.
unsafe impl Send for Encoder {}

fn message(error: &[u8]) -> String {
    let end = error.iter().position(|&c| c == 0).unwrap_or(error.len());
    format!("NVENC: {}", String::from_utf8_lossy(&error[..end]))
}

impl Encoder {
    pub(super) fn new(
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        size: (u32, u32),
        limits: SendLimits,
    ) -> Result<Self> {
        let mut error = [0u8; 192];
        let session = unsafe {
            rc_nvenc_open(
                device.as_raw(),
                texture.as_raw(),
                size.0,
                size.1,
                limits.fps,
                limits.max_bitrate,
                error.as_mut_ptr().cast(),
                error.len() as u32,
            )
        };
        Ok(Self {
            session: NonNull::new(session).ok_or_else(|| message(&error))?,
            start: None,
            _texture: texture.clone(),
            _device: device.clone(),
        })
    }

    pub(super) fn encode(&mut self, keyframe: bool, at: Instant) -> Result<Option<EncodedFrame>> {
        let start = *self.start.get_or_insert(at);
        let timestamp = at
            .saturating_duration_since(start)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let mut packet = Packet {
            data: ptr::null(),
            length: 0,
            keyframe: 0,
        };
        let mut error = [0u8; 192];
        let ok = unsafe {
            rc_nvenc_encode(
                self.session.as_ptr(),
                timestamp,
                i32::from(keyframe),
                &mut packet,
                error.as_mut_ptr().cast(),
                error.len() as u32,
            )
        };
        if ok == 0 {
            return Err(message(&error));
        }
        let result = (packet.length > 0 && !packet.data.is_null()).then(|| {
            // The driver owns the packet until release; the network owns its own copy.
            let data = unsafe { std::slice::from_raw_parts(packet.data, packet.length as usize) };
            EncodedFrame {
                annexb: data.to_vec(),
                captured: at,
                keyframe: packet.keyframe != 0,
            }
        });
        let ok = unsafe {
            rc_nvenc_release(
                self.session.as_ptr(),
                error.as_mut_ptr().cast(),
                error.len() as u32,
            )
        };
        if ok == 0 {
            return Err(message(&error));
        }
        Ok(result)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { rc_nvenc_close(self.session.as_ptr()) };
    }
}

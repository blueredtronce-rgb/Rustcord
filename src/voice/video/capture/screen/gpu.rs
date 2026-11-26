//! Convert the captured surface to NV12 on its adapter and encode it on NVIDIA hardware.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
};
use windows::core::Interface;
use windows_capture::d3d11::StagingTexture;

use super::{Pipeline, cursor::Cursor, nvenc, scale::Scale};
use crate::voice::video::{EncodedFrame, SendLimits, codec, trace::Metric};

const NVIDIA: u32 = 0x10de;
type Result<T> = std::result::Result<T, String>;

pub(super) struct Hardware {
    encoder: nvenc::Encoder,
    scale: Scale,
    transfer: Option<Transfer>,
    cursor_layer: Option<CursorLayer>,
    preview: Option<Preview>,
    size: (u32, u32),
    format: DXGI_FORMAT,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
}

impl Hardware {
    pub(super) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        size: (u32, u32),
        format: DXGI_FORMAT,
        limits: SendLimits,
    ) -> Result<Self> {
        let target = codec::fit_within(size.0, size.1, limits.max_width, limits.max_height);
        let capture_vendor = unsafe {
            device
                .cast::<IDXGIDevice>()
                .and_then(|dxgi| dxgi.GetAdapter())
                .and_then(|adapter| adapter.GetDesc())
        }
        .map_err(|e| e.to_string())?
        .VendorId;
        let nvidia = if capture_vendor != NVIDIA {
            Some(nvidia_device()?)
        } else {
            None
        };
        let scale = Scale::convert(
            device,
            context,
            size,
            target,
            (format, DXGI_FORMAT_NV12),
            limits.fps,
        )
        .map_err(|e| e.to_string())?;
        let (encoder, transfer) = if let Some((encode_device, encode_context)) = nvidia {
            let transfer = Transfer {
                staging: StagingTexture::new(device, target.0, target.1, DXGI_FORMAT_NV12)
                    .map_err(|e| e.to_string())?,
                destination: texture(
                    &encode_device,
                    target,
                    DXGI_FORMAT_NV12,
                    D3D11_BIND_RENDER_TARGET,
                )?,
                context: encode_context,
                height: target.1,
            };
            let encoder =
                nvenc::Encoder::new(&encode_device, &transfer.destination, target, limits)?;
            (encoder, Some(transfer))
        } else {
            (
                nvenc::Encoder::new(device, scale.output_texture(), target, limits)?,
                None,
            )
        };
        Ok(Self {
            encoder,
            scale,
            transfer,
            cursor_layer: None,
            preview: None,
            size,
            format,
            context: context.clone(),
            device: device.clone(),
        })
    }

    pub(super) fn label(&self) -> &'static str {
        if self.transfer.is_some() {
            "NVENC, NV12 transfer between GPUs"
        } else {
            "NVENC, direct GPU textures"
        }
    }

    pub(super) fn prepare(
        &mut self,
        frame: &ID3D11Texture2D,
        cursor: Option<&Cursor>,
        pipe: &mut Pipeline,
    ) -> Result<()> {
        let started = pipe.trace.as_ref().map(|_| Instant::now());
        let frame = if let Some(cursor) = cursor
            && let Some(bounds) = cursor.bounds(self.size)
        {
            if self.cursor_layer.is_none() {
                self.cursor_layer = Some(CursorLayer {
                    image: texture(
                        &self.device,
                        self.size,
                        self.format,
                        D3D11_BIND_RENDER_TARGET,
                    )?,
                    staging: None,
                });
            }
            let layer = self
                .cursor_layer
                .as_mut()
                .expect("cursor layer initialized");
            layer.paint(
                &self.device,
                &self.context,
                self.size,
                self.format,
                frame,
                cursor,
                bounds,
            )?
        } else {
            frame
        };
        self.scale.frame(frame).map_err(|e| e.to_string())?;
        if let (Some(trace), Some(started)) = (&mut pipe.trace, started) {
            trace.record(Metric::GpuConvert, started.elapsed());
        }
        if let Some(transfer) = &self.transfer {
            let started = pipe.trace.as_ref().map(|_| Instant::now());
            transfer.upload(&self.context, self.scale.output_texture())?;
            if let (Some(trace), Some(started)) = (&mut pipe.trace, started) {
                trace.record(Metric::GpuTransfer, started.elapsed());
            }
        }
        Ok(())
    }

    pub(super) fn encode(&mut self, keyframe: bool, now: Instant) -> Result<Option<EncodedFrame>> {
        self.encoder.encode(keyframe, now)
    }

    pub(super) fn preview(&mut self, pipe: &mut Pipeline, now: Instant, idle: bool) {
        use std::time::Duration;
        if pipe.next_preview.is_some_and(|next| now < next) {
            return;
        }
        let mut state = crate::voice::video::lock(&pipe.preview);
        if idle && !state.refresh {
            return;
        }
        let Some(writer) = state.writer.as_ref() else {
            return;
        };
        if self.preview.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { self.scale.output_texture().GetDesc(&mut desc) };
            let source = (desc.Width, desc.Height);
            let size = codec::fit_within(source.0, source.1, 640, 360);
            self.preview = Preview::new(&self.device, &self.context, source, size).ok();
        }
        let Some(picture) = self.preview.as_ref() else {
            pipe.next_preview = Some(now + Duration::from_secs(2));
            return;
        };
        let started = pipe.trace.as_ref().map(|_| Instant::now());
        if picture
            .draw(&self.context, self.scale.output_texture(), writer)
            .is_ok()
        {
            state.refresh = false;
            if let (Some(trace), Some(started)) = (&mut pipe.trace, started) {
                trace.record(Metric::Preview, started.elapsed());
            }
        }
        pipe.next_preview = Some(now + Duration::from_millis(200));
    }
}

pub(super) fn device_on(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| e.to_string())?;
    Ok((
        device.ok_or("No Direct3D device")?,
        context.ok_or("No Direct3D context")?,
    ))
}

fn nvidia_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| e.to_string())?;
    for index in 0..32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let properties = unsafe { adapter.GetDesc1() }.map_err(|e| e.to_string())?;
        if properties.VendorId == NVIDIA {
            return device_on(&adapter);
        }
    }
    Err("No NVIDIA encoding adapter".into())
}

fn texture(
    device: &ID3D11Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
    flags: D3D11_BIND_FLAG,
) -> Result<ID3D11Texture2D> {
    let mut result = None;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: flags.0 as u32,
        ..Default::default()
    };
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut result)) }.map_err(|e| e.to_string())?;
    result.ok_or_else(|| "No encoder texture".into())
}

struct Transfer {
    staging: StagingTexture,
    destination: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    height: u32,
}

impl Transfer {
    fn upload(&self, capture: &ID3D11DeviceContext, input: &ID3D11Texture2D) -> Result<()> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // A hybrid system cannot share these D3D11 textures between adapters. Copy only the
        // already converted NV12 planes, preserving the driver's row pitch for both planes.
        unsafe {
            capture.CopyResource(self.staging.texture(), input);
            capture
                .Map(
                    self.staging.texture(),
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )
                .map_err(|e| e.to_string())?;
            let pitch = mapped.RowPitch;
            self.context.UpdateSubresource(
                &self.destination,
                0,
                None,
                mapped.pData,
                pitch,
                pitch.saturating_mul(self.height + self.height / 2),
            );
            capture.Unmap(self.staging.texture(), 0);
        }
        Ok(())
    }
}

struct CursorLayer {
    image: ID3D11Texture2D,
    staging: Option<StagingTexture>,
}

impl CursorLayer {
    fn paint(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        size: (u32, u32),
        format: DXGI_FORMAT,
        frame: &ID3D11Texture2D,
        cursor: &Cursor,
        bounds: (u32, u32, u32, u32),
    ) -> Result<&ID3D11Texture2D> {
        let (left, top, right, bottom) = bounds;
        let (w, h) = (right - left, bottom - top);
        // Keep the staging texture while the cursor moves or is clipped at screen edges.
        if self
            .staging
            .as_ref()
            .is_none_or(|s| s.desc().Width < w || s.desc().Height < h)
        {
            self.staging = Some(
                StagingTexture::new(device, w.max(64), h.max(64), format)
                    .map_err(|e| e.to_string())?,
            );
        }
        let staging = self
            .staging
            .as_ref()
            .expect("cursor staging initialized")
            .texture();
        let region = D3D11_BOX {
            left,
            top,
            front: 0,
            right,
            bottom,
            back: 1,
        };
        let local = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: w,
            bottom: h,
            back: 1,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // Read only the small cursor rectangle for alpha/XOR, then put it back into the GPU copy.
        unsafe {
            context.CopySubresourceRegion(staging, 0, 0, 0, 0, frame, 0, Some(&region));
            context
                .Map(staging, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))
                .map_err(|e| e.to_string())?;
            let stride = mapped.RowPitch as usize;
            let data =
                std::slice::from_raw_parts_mut(mapped.pData.cast::<u8>(), stride * h as usize);
            cursor.paint_region(size, size, bounds, stride, data);
            context.Unmap(staging, 0);
            context.CopyResource(&self.image, frame);
            context.CopySubresourceRegion(&self.image, 0, left, top, 0, staging, 0, Some(&local));
        }
        Ok(&self.image)
    }
}

struct Preview {
    scale: Scale,
    staging: StagingTexture,
    size: (u32, u32),
}

impl Preview {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: (u32, u32),
        size: (u32, u32),
    ) -> Result<Self> {
        let format = DXGI_FORMAT_B8G8R8A8_UNORM;
        Ok(Self {
            scale: Scale::convert(device, context, source, size, (DXGI_FORMAT_NV12, format), 5)
                .map_err(|e| e.to_string())?,
            staging: StagingTexture::new(device, size.0, size.1, format)
                .map_err(|e| e.to_string())?,
            size,
        })
    }

    fn draw(
        &self,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        writer: &crate::voice::video::FrameWriter,
    ) -> Result<()> {
        self.scale.frame(source).map_err(|e| e.to_string())?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let (w, h) = self.size;
        unsafe {
            context.CopyResource(self.staging.texture(), self.scale.output_texture());
            context
                .Map(
                    self.staging.texture(),
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )
                .map_err(|e| e.to_string())?;
            let stride = mapped.RowPitch as usize;
            let data = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), stride * h as usize);
            writer.write(w, h, |rgba| {
                let row = w as usize * 4;
                for (source, out) in data.chunks_exact(stride).zip(rgba.chunks_exact_mut(row)) {
                    for (bgra, dst) in source[..row].chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                        dst.copy_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
                    }
                }
            });
            context.Unmap(self.staging.texture(), 0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Id;
    use crate::voice::video::{StreamKind, VideoFrames, VideoKey};
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    #[ignore = "requires an NVIDIA encoder and Direct3D 11 on the test machine"]
    fn real_gpu_stream_frames_decode_at_both_resolutions() {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.unwrap();
        let mut checked_nvidia = false;
        for index in 0..16 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            let desc = unsafe { adapter.GetDesc1() }.unwrap();
            if !matches!(desc.VendorId, NVIDIA | 0x1002 | 0x8086) {
                continue;
            }
            let (device, context) = device_on(&adapter).unwrap();
            let size = (1920, 1080);
            let format = DXGI_FORMAT_B8G8R8A8_UNORM;
            let input = texture(&device, size, format, D3D11_BIND_RENDER_TARGET).unwrap();
            let pixels = [25u8, 80, 205, 255].repeat((size.0 * size.1) as usize);
            unsafe {
                context.UpdateSubresource(&input, 0, None, pixels.as_ptr().cast(), size.0 * 4, 0);
            }
            let pattern: Vec<u8> = (0..320 * 240)
                .flat_map(|i| {
                    if ((i / 320) / 8 + (i % 320) / 8) % 2 == 0 {
                        [245u8, 130, 20, 255]
                    } else {
                        [45u8, 20, 230, 255]
                    }
                })
                .collect();
            for height in [720, 1080] {
                let limits = SendLimits::screen(30, height);
                let (sender, _receiver) = mpsc::channel(2);
                let mut pipe = Pipeline::new(
                    limits,
                    StreamKind::Screen,
                    sender,
                    Arc::new(AtomicBool::default()),
                    Arc::new(Mutex::new(Default::default())),
                    Arc::new(AtomicBool::default()),
                );
                let mut hardware = Hardware::new(&device, &context, size, format, limits).unwrap();
                let frames = VideoFrames::default();
                let key = VideoKey::User(Id(1));
                let writer = frames.writer(key.clone()).unwrap();
                let mut decoder = codec::Decoder::new().unwrap();
                let mut elapsed = Duration::ZERO;
                let start = Instant::now();
                for i in 0..60 {
                    let left = i * 13;
                    let rect = D3D11_BOX {
                        left,
                        top: 16,
                        front: 0,
                        right: left + 320,
                        bottom: 256,
                        back: 1,
                    };
                    unsafe {
                        context.UpdateSubresource(
                            &input,
                            0,
                            Some(&rect),
                            pattern.as_ptr().cast(),
                            320 * 4,
                            0,
                        );
                    }
                    let before = Instant::now();
                    hardware.prepare(&input, None, &mut pipe).unwrap();
                    let frame = hardware
                        .encode(
                            i == 0 || i == 45,
                            start + Duration::from_millis(u64::from(i) * 1000 / 30),
                        )
                        .unwrap()
                        .unwrap();
                    elapsed += before.elapsed();
                    if i == 0 || i == 45 {
                        assert!(frame.keyframe);
                        assert!(decoder.decode(&frame.annexb, &writer).unwrap().is_some());
                        let (_, image) = frames.latest(&key).unwrap();
                        let pixel =
                            image.pixels[image.size[0] * (image.size[1] / 2) + image.size[0] / 2];
                        assert!(
                            pixel.r() > 120 && pixel.r() > pixel.g() + 30 && pixel.g() > pixel.b(),
                            "RGB -> NV12 -> H.264 must preserve the test color: {pixel:?}"
                        );
                    } else {
                        assert!(!frame.annexb.is_empty());
                    }
                }
                let mode = hardware.label();
                eprintln!(
                    "{} vendor {:#06x}, {height}p: 60/60 frames, conversion + encode {:.2} ms/frame",
                    mode,
                    desc.VendorId,
                    elapsed.as_secs_f64() * 1000.0 / 60.0
                );
            }
            checked_nvidia |= desc.VendorId == NVIDIA;
        }
        assert!(checked_nvidia, "No NVIDIA device was tested");
    }
}

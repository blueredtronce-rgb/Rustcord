//! Resize the capture surface before copying it out of GPU memory.

use std::mem::ManuallyDrop;
use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::core::{Error, Interface, Result};

pub(super) struct Scale {
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    target: ID3D11Texture2D,
    output: ID3D11VideoProcessorOutputView,
}

impl Scale {
    pub(super) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: (u32, u32),
        dst: (u32, u32),
        format: DXGI_FORMAT,
        fps: u32,
    ) -> Result<Self> {
        Self::convert(device, context, src, dst, (format, format), fps)
    }

    pub(super) fn convert(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: (u32, u32),
        dst: (u32, u32),
        formats: (DXGI_FORMAT, DXGI_FORMAT),
        fps: u32,
    ) -> Result<Self> {
        let video: ID3D11VideoDevice = device.cast()?;
        let context: ID3D11VideoContext = context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src.0,
            InputHeight: src.1,
            OutputFrameRate: rate,
            OutputWidth: dst.0,
            OutputHeight: dst.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&content)? };
        let input = D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32;
        let output_flag = D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32;
        if unsafe { enumerator.CheckVideoProcessorFormat(formats.0)? } & input == 0
            || unsafe { enumerator.CheckVideoProcessorFormat(formats.1)? } & output_flag == 0 {
            return Err(Error::from(E_FAIL));
        }
        let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0)? };
        let mut target = None;
        let mut output = None;
        let rect = |(w, h)| RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        let target_desc = D3D11_TEXTURE2D_DESC {
            Width: dst.0,
            Height: dst.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: formats.1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let target = unsafe {
            device.CreateTexture2D(&target_desc, None, Some(&mut target))?;
            let target = target.ok_or_else(|| Error::from(E_FAIL))?;
            video.CreateVideoProcessorOutputView(
                &target,
                &enumerator,
                &view_desc,
                Some(&mut output),
            )?;
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&rect(src)));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect(dst)));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect(dst)));
            target
        };
        Ok(Self {
            video,
            context,
            enumerator,
            processor,
            target,
            output: output.ok_or_else(|| Error::from(E_FAIL))?,
        })
    }

    pub(super) fn frame(&self, texture: &ID3D11Texture2D) -> Result<()> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut view = None;
        unsafe {
            self.video.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )?
        };
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(view),
            ..Default::default()
        };
        // The generated stream struct borrows this COM reference through VideoProcessorBlt.
        unsafe {
            let result = self.context.VideoProcessorBlt(
                &self.processor,
                &self.output,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result
        }
    }

    pub(super) fn output_texture(&self) -> &ID3D11Texture2D {
        &self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows_capture::d3d11::{StagingTexture, create_d3d_device};

    #[test]
    #[ignore = "requires a Windows device with a D3D11 video processor"]
    fn downscales_a_capture_surface_on_the_local_gpu() {
        let (device, context) = create_d3d_device().unwrap();
        let format = DXGI_FORMAT_B8G8R8A8_UNORM;
        let scale = Scale::new(&device, &context, (320, 180), (160, 90), format, 30).unwrap();
        let pixels = [48u8, 112, 196, 255].repeat(320 * 180);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 320,
            Height: 180,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let input = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: 320 * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(&desc, Some(&input), Some(&mut texture))
                .unwrap()
        };
        scale.frame(&texture.unwrap()).unwrap();
        let staging = StagingTexture::new(&device, 160, 90, format).unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let center = unsafe {
            context.CopyResource(staging.texture(), scale.output_texture());
            context
                .Map(staging.texture(), 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let p = mapped
                .pData
                .cast::<u8>()
                .add(45 * mapped.RowPitch as usize + 80 * 4);
            let pixel = p.cast::<[u8; 4]>().read();
            context.Unmap(staging.texture(), 0);
            pixel
        };
        assert!(
            center[..3]
                .iter()
                .zip([48, 112, 196])
                .all(|(&a, b)| a.abs_diff(b) <= 4),
            "unexpected color: {center:?}"
        );
    }
}

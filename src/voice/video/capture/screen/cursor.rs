//! A DXGI hardware pointer may arrive separately from the desktop texture.

use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows_capture::dxgi_duplication_api::DxgiDuplicationFrame;

#[derive(Default)]
pub(super) struct Cursor {
    position: Option<(i32, i32)>,
    shape: Option<DXGI_OUTDUPL_POINTER_SHAPE_INFO>,
    pixels: Vec<u8>,
}

impl Cursor {
    pub(super) fn update(&mut self, frame: &DxgiDuplicationFrame<'_>) {
        let info = frame.frame_info();
        if info.LastMouseUpdateTime != 0 {
            let pointer = info.PointerPosition;
            self.position = pointer
                .Visible
                .as_bool()
                .then_some((pointer.Position.x, pointer.Position.y));
        }
        let size = info.PointerShapeBufferSize;
        if size == 0 {
            return;
        }
        self.shape = None;
        if size > 1024 * 1024 {
            return;
        }
        self.pixels.resize(size as usize, 0);
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut written = 0;
        // The frame owns the acquired DXGI surface and the provided buffer has size bytes.
        let read = unsafe {
            frame.duplication().GetFramePointerShape(
                size,
                self.pixels.as_mut_ptr().cast(),
                &mut written,
                &mut shape,
            )
        };
        if read.is_ok() && written <= size {
            self.pixels.truncate(written as usize);
            self.shape = Some(shape);
        }
    }

    pub(super) fn paint(
        &self,
        source: (u32, u32),
        target: (u32, u32),
        stride: usize,
        frame: &mut [u8],
    ) {
        self.paint_region(source, target, (0, 0, target.0, target.1), stride, frame);
    }

    /// Bounds of the separately reported cursor, clipped to the original texture.
    pub(super) fn bounds(&self, size: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
        let (x, y) = self.position?;
        let s = self.shape?;
        let rows = if s.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 { s.Height / 2 } else { s.Height };
        let clamp = |v: i64, max| v.clamp(0, i64::from(max)) as u32;
        let (left, top) = (clamp(i64::from(x), size.0), clamp(i64::from(y), size.1));
        let (right, bottom) = (clamp(i64::from(x) + i64::from(s.Width), size.0), clamp(i64::from(y) + i64::from(rows), size.1));
        (left < right && top < bottom).then_some((left, top, right, bottom))
    }

    pub(super) fn paint_region(
        &self,
        source: (u32, u32),
        target: (u32, u32),
        region: (u32, u32, u32, u32),
        stride: usize,
        frame: &mut [u8],
    ) {
        let (width, height) = target;
        let (x0, y0, x1, y1) = region;
        let (Some((x, y)), Some(shape)) = (self.position, self.shape) else {
            return;
        };
        let color = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
        let masked = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
        let monochrome = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let w = shape.Width as usize;
        let rows = shape.Height as usize;
        let pitch = shape.Pitch as usize;
        let visible_rows = if shape.Type == monochrome {
            if !rows.is_multiple_of(2) || pitch < w.div_ceil(8) {
                return;
            }
            rows / 2
        } else {
            if (shape.Type != color && shape.Type != masked) || w > pitch / 4 {
                return;
            }
            rows
        };
        if pitch
            .checked_mul(rows)
            .is_none_or(|len| len > self.pixels.len())
            || stride < (x1 - x0) as usize * 4
            || stride
                .checked_mul((y1 - y0) as usize)
                .is_none_or(|len| len > frame.len())
        {
            return;
        }
        let x = i64::from(x);
        let y = i64::from(y);
        let (sw, sh) = (i64::from(source.0.max(1)), i64::from(source.1.max(1)));
        let (tw, th) = (i64::from(width), i64::from(height));
        let ceil =
            |position: i64, target: i64, source: i64| -(-position * target).div_euclid(source);
        let left = ceil(x, tw, sw).clamp(i64::from(x0), i64::from(x1));
        let top = ceil(y, th, sh).clamp(i64::from(y0), i64::from(y1));
        let right = ceil(x + w as i64, tw, sw).clamp(left, i64::from(x1));
        let bottom = ceil(y + visible_rows as i64, th, sh).clamp(top, i64::from(y1));
        for dst_y in top..bottom {
            let row = (dst_y * sh / th - y) as usize * pitch;
            for dst_x in left..right {
                let src_x = (dst_x * sw / tw - x) as usize;
                let offset = (dst_y as usize - y0 as usize) * stride + (dst_x as usize - x0 as usize) * 4;
                let dst = &mut frame[offset..offset + 3];
                if shape.Type == monochrome {
                    let bit = 0x80 >> (src_x % 8);
                    let index = row + src_x / 8;
                    let and = if self.pixels[index] & bit != 0 {
                        255
                    } else {
                        0
                    };
                    let xor = if self.pixels[index + visible_rows * pitch] & bit != 0 {
                        255
                    } else {
                        0
                    };
                    for channel in dst {
                        *channel = (*channel & and) ^ xor;
                    }
                } else {
                    let p = row + src_x * 4;
                    let src = &self.pixels[p..p + 4];
                    if shape.Type == masked {
                        if src[3] == 0 {
                            dst.copy_from_slice(&src[..3]);
                        } else {
                            for (to, from) in dst.iter_mut().zip(src) {
                                *to ^= *from;
                            }
                        }
                    } else {
                        let alpha = u16::from(src[3]);
                        for (to, from) in dst.iter_mut().zip(src) {
                            *to =
                                ((u16::from(*from) * alpha + u16::from(*to) * (255 - alpha) + 127)
                                    / 255) as u8;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_color_cursor_at_edges_and_keeps_alpha() {
        let cursor = Cursor {
            position: Some((-1, 0)),
            shape: Some(DXGI_OUTDUPL_POINTER_SHAPE_INFO {
                Type: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
                Width: 2,
                Height: 1,
                Pitch: 8,
                ..Default::default()
            }),
            pixels: vec![255, 255, 255, 255, 110, 120, 130, 128],
        };
        let mut frame = [10, 20, 30, 255, 10, 20, 30, 255];
        cursor.paint((2, 1), (2, 1), 8, &mut frame);
        assert_eq!(frame, [60, 70, 80, 255, 10, 20, 30, 255]);
    }

    #[test]
    fn monochrome_and_masked_cursors_honor_xor_masks() {
        let make = |shape_type, height, pitch, pixels| Cursor {
            position: Some((0, 0)),
            shape: Some(DXGI_OUTDUPL_POINTER_SHAPE_INFO {
                Type: shape_type,
                Width: 2,
                Height: height,
                Pitch: pitch,
                ..Default::default()
            }),
            pixels,
        };
        let mut frame = [10, 20, 30, 255, 40, 50, 60, 255];
        let mono = make(
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32,
            2,
            1,
            vec![0b1000_0000, 0b1000_0000],
        );
        mono.paint((2, 1), (2, 1), 8, &mut frame);
        assert_eq!(frame, [245, 235, 225, 255, 0, 0, 0, 255]);
        let masked = make(
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32,
            1,
            8,
            vec![255, 255, 255, 255, 1, 2, 3, 0],
        );
        masked.paint((2, 1), (2, 1), 8, &mut frame);
        assert_eq!(frame, [10, 20, 30, 255, 1, 2, 3, 255]);
    }
}

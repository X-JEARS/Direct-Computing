use crate::{FrameSink, PixelFormat, VideoFrame};
use dc_common::{DcError, Result};
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Debug, Default)]
pub struct LastFrameSink {
    frames_presented: u64,
    last_frame: Option<VideoFrame>,
}

impl LastFrameSink {
    pub const fn frames_presented(&self) -> u64 {
        self.frames_presented
    }

    pub const fn last_frame(&self) -> Option<&VideoFrame> {
        self.last_frame.as_ref()
    }

    pub fn into_last_frame(self) -> Option<VideoFrame> {
        self.last_frame
    }
}

impl FrameSink for LastFrameSink {
    fn present(&mut self, frame: VideoFrame) -> Result<()> {
        self.frames_presented += 1;
        self.last_frame = Some(frame);
        Ok(())
    }
}

/// Writes a packed RGB or BGRA frame as a 32-bit BMP image for visual diagnostics.
pub fn write_bmp(path: impl AsRef<Path>, frame: &VideoFrame) -> Result<()> {
    let layout = frame.layout();
    if layout.pixel_format() == PixelFormat::I420 {
        return Err(DcError::Unsupported(
            "BMP output does not support planar I420 frames".into(),
        ));
    }

    let width = layout.size().width() as usize;
    let height = layout.size().height() as usize;
    let width_i32 =
        i32::try_from(width).map_err(|_| DcError::InvalidInput("BMP width exceeds i32".into()))?;
    let height_i32 = i32::try_from(height)
        .map_err(|_| DcError::InvalidInput("BMP height exceeds i32".into()))?;
    let pixel_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| DcError::InvalidInput("BMP pixel data is too large".into()))?;
    let file_size = 54_u32
        .checked_add(pixel_bytes)
        .ok_or_else(|| DcError::InvalidInput("BMP file size overflowed".into()))?;

    let file = std::fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(b"BM")?;
    writer.write_all(&file_size.to_le_bytes())?;
    writer.write_all(&[0; 4])?;
    writer.write_all(&54_u32.to_le_bytes())?;
    writer.write_all(&40_u32.to_le_bytes())?;
    writer.write_all(&width_i32.to_le_bytes())?;
    writer.write_all(&height_i32.to_le_bytes())?;
    writer.write_all(&1_u16.to_le_bytes())?;
    writer.write_all(&32_u16.to_le_bytes())?;
    writer.write_all(&0_u32.to_le_bytes())?;
    writer.write_all(&pixel_bytes.to_le_bytes())?;
    writer.write_all(&2_835_i32.to_le_bytes())?;
    writer.write_all(&2_835_i32.to_le_bytes())?;
    writer.write_all(&0_u32.to_le_bytes())?;
    writer.write_all(&0_u32.to_le_bytes())?;

    for row in frame.data().chunks_exact(layout.stride()).rev() {
        match layout.pixel_format() {
            PixelFormat::Rgb24 => {
                for pixel in row[..width * 3].chunks_exact(3) {
                    writer.write_all(&[pixel[2], pixel[1], pixel[0], u8::MAX])?;
                }
            }
            PixelFormat::Bgra32 => {
                writer.write_all(&row[..width * 4])?;
            }
            PixelFormat::I420 => unreachable!("I420 is rejected above"),
        }
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameLayout, FrameSize};
    use std::time::Duration;

    #[test]
    fn last_frame_sink_keeps_only_the_latest_frame() {
        let size = FrameSize::new(2, 2).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Rgb24).unwrap();
        let mut sink = LastFrameSink::default();
        sink.present(VideoFrame::new(4, Duration::ZERO, layout, vec![1; 12]).unwrap())
            .unwrap();
        sink.present(VideoFrame::new(5, Duration::ZERO, layout, vec![2; 12]).unwrap())
            .unwrap();
        assert_eq!(sink.frames_presented(), 2);
        assert_eq!(sink.last_frame().unwrap().sequence(), 5);
    }

    #[test]
    fn writes_a_standard_bmp_header() {
        let size = FrameSize::new(2, 2).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Rgb24).unwrap();
        let frame = VideoFrame::new(0, Duration::ZERO, layout, vec![128; 12]).unwrap();
        let path =
            std::env::temp_dir().join(format!("direct-computing-media-{}.bmp", std::process::id()));
        write_bmp(&path, &frame).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(&bytes[..2], b"BM");
        assert_eq!(bytes.len(), 54 + 2 * 2 * 4);
        assert_eq!(u32::from_le_bytes(bytes[10..14].try_into().unwrap()), 54);
    }
}

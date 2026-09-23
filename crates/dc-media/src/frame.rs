use dc_common::{DcError, Result};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorShapeKind {
    Monochrome,
    Color,
    MaskedColor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorShape {
    pub kind: CursorShapeKind,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorUpdate {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub shape: Option<CursorShape>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameMetadata {
    /// `Some` means the source supplied authoritative damage metadata. An
    /// empty vector therefore means a cursor-only or unchanged desktop frame.
    pub damage: Option<Vec<DamageRect>>,
    pub cursor: Option<CursorUpdate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Rgb24,
    Bgra32,
    I420,
}

impl PixelFormat {
    pub const fn packed_bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Rgb24 => Some(3),
            Self::Bgra32 => Some(4),
            Self::I420 => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameSize {
    width: u32,
    height: u32,
}

impl FrameSize {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(DcError::InvalidInput(
                "frame width and height must be non-zero".into(),
            ));
        }
        Ok(Self { width, height })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    size: FrameSize,
    pixel_format: PixelFormat,
    stride: usize,
    data_len: usize,
}

impl FrameLayout {
    pub fn packed(size: FrameSize, pixel_format: PixelFormat) -> Result<Self> {
        let bytes_per_pixel = pixel_format.packed_bytes_per_pixel().ok_or_else(|| {
            DcError::Unsupported("I420 requires explicit plane layout support".into())
        })?;
        let stride = usize::try_from(size.width())
            .ok()
            .and_then(|width| width.checked_mul(bytes_per_pixel))
            .ok_or_else(|| DcError::InvalidInput("frame row size overflows usize".into()))?;
        Self::with_stride(size, pixel_format, stride)
    }

    pub fn with_stride(size: FrameSize, pixel_format: PixelFormat, stride: usize) -> Result<Self> {
        let bytes_per_pixel = pixel_format.packed_bytes_per_pixel().ok_or_else(|| {
            DcError::Unsupported("I420 requires explicit plane layout support".into())
        })?;
        let minimum_stride = usize::try_from(size.width())
            .ok()
            .and_then(|width| width.checked_mul(bytes_per_pixel))
            .ok_or_else(|| DcError::InvalidInput("frame row size overflows usize".into()))?;
        if stride < minimum_stride {
            return Err(DcError::InvalidInput(format!(
                "frame stride {stride} is smaller than packed row size {minimum_stride}"
            )));
        }
        let data_len = stride
            .checked_mul(size.height() as usize)
            .ok_or_else(|| DcError::InvalidInput("frame buffer size overflows usize".into()))?;
        Ok(Self {
            size,
            pixel_format,
            stride,
            data_len,
        })
    }

    pub const fn size(self) -> FrameSize {
        self.size
    }

    pub const fn pixel_format(self) -> PixelFormat {
        self.pixel_format
    }

    pub const fn stride(self) -> usize {
        self.stride
    }

    pub const fn data_len(self) -> usize {
        self.data_len
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFrame {
    sequence: u64,
    timestamp: Duration,
    layout: FrameLayout,
    data: Vec<u8>,
    metadata: FrameMetadata,
}

impl VideoFrame {
    pub fn new(
        sequence: u64,
        timestamp: Duration,
        layout: FrameLayout,
        data: Vec<u8>,
    ) -> Result<Self> {
        if data.len() != layout.data_len() {
            return Err(DcError::InvalidInput(format!(
                "frame buffer has {} bytes, expected {}",
                data.len(),
                layout.data_len()
            )));
        }
        Ok(Self {
            sequence,
            timestamp,
            layout,
            data,
            metadata: FrameMetadata::default(),
        })
    }

    pub fn with_metadata(mut self, metadata: FrameMetadata) -> Result<Self> {
        if let Some(rects) = &metadata.damage {
            let size = self.layout.size();
            for rect in rects {
                let right = rect.x.checked_add(rect.width);
                let bottom = rect.y.checked_add(rect.height);
                if rect.width == 0
                    || rect.height == 0
                    || right.is_none_or(|value| value > size.width())
                    || bottom.is_none_or(|value| value > size.height())
                {
                    return Err(DcError::InvalidInput(
                        "damage rectangle is outside the frame".into(),
                    ));
                }
            }
        }
        self.metadata = metadata;
        Ok(self)
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn timestamp(&self) -> Duration {
        self.timestamp
    }

    pub const fn layout(&self) -> FrameLayout {
        self.layout
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn metadata(&self) -> &FrameMetadata {
        &self.metadata
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_sized_frames() {
        assert!(FrameSize::new(0, 10).is_err());
        assert!(FrameSize::new(10, 0).is_err());
    }

    #[test]
    fn calculates_packed_layout() {
        let size = FrameSize::new(640, 480).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        assert_eq!(layout.stride(), 2_560);
        assert_eq!(layout.data_len(), 1_228_800);
    }

    #[test]
    fn rejects_an_invalid_buffer_length() {
        let size = FrameSize::new(2, 2).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Rgb24).unwrap();
        let result = VideoFrame::new(0, Duration::ZERO, layout, vec![0; 11]);
        assert!(result.is_err());
    }
}

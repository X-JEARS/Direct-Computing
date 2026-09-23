use dc_common::{DcError, Result};
use dc_media::{DamageRect, PixelFormat, VideoFrame};
use dc_protocol::{DesktopRect, WireMessage, MAX_DESKTOP_REGIONS};

pub const REGION_ENCODING_RAW: u8 = 0;
pub const REGION_ENCODING_PACK_BITS: u8 = 1;
const DEFAULT_TILE_SIZE: u32 = 64;

pub struct DirtyRegionDetector {
    previous: Vec<u8>,
    tile_size: u32,
    dimensions: Option<(u32, u32, usize)>,
}

impl Default for DirtyRegionDetector {
    fn default() -> Self {
        Self {
            previous: Vec::new(),
            tile_size: DEFAULT_TILE_SIZE,
            dimensions: None,
        }
    }
}

impl DirtyRegionDetector {
    pub fn detect(&mut self, frame: &VideoFrame) -> Result<Vec<DamageRect>> {
        let layout = frame.layout();
        let bytes_per_pixel = layout
            .pixel_format()
            .packed_bytes_per_pixel()
            .ok_or_else(|| DcError::Unsupported("dirty detection requires packed pixels".into()))?;
        let size = layout.size();
        let dimensions = (size.width(), size.height(), layout.stride());
        // DXGI can coalesce metadata into a full-screen rectangle (and some
        // drivers report an empty list for a texture that still changed).
        // Compare the copied texture as a fallback/verification step so a
        // broad or empty metadata list never disables region updates.
        let regions =
            if self.dimensions != Some(dimensions) || self.previous.len() != frame.data().len() {
                vec![DamageRect {
                    x: 0,
                    y: 0,
                    width: size.width(),
                    height: size.height(),
                }]
            } else {
                detect_changed_tiles(
                    &self.previous,
                    frame.data(),
                    size.width(),
                    size.height(),
                    layout.stride(),
                    bytes_per_pixel,
                    self.tile_size,
                )
            };
        self.previous.clear();
        self.previous.extend_from_slice(frame.data());
        self.dimensions = Some(dimensions);
        Ok(coalesce_regions(regions, MAX_DESKTOP_REGIONS))
    }
}

fn detect_changed_tiles(
    previous: &[u8],
    current: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    bytes_per_pixel: usize,
    tile_size: u32,
) -> Vec<DamageRect> {
    let mut regions = Vec::new();
    let tiles_x = width.div_ceil(tile_size);
    let tiles_y = height.div_ceil(tile_size);
    for tile_y in 0..tiles_y {
        let y = tile_y * tile_size;
        let tile_height = tile_size.min(height - y);
        let mut run_start = None;
        for tile_x in 0..=tiles_x {
            let changed = if tile_x == tiles_x {
                false
            } else {
                let x = tile_x * tile_size;
                let tile_width = tile_size.min(width - x);
                (0..tile_height).any(|row| {
                    let start = (y + row) as usize * stride + x as usize * bytes_per_pixel;
                    let end = start + tile_width as usize * bytes_per_pixel;
                    previous[start..end] != current[start..end]
                })
            };
            match (run_start, changed) {
                (None, true) => run_start = Some(tile_x),
                (Some(start), false) => {
                    let x = start * tile_size;
                    regions.push(DamageRect {
                        x,
                        y,
                        width: (tile_x * tile_size).min(width) - x,
                        height: tile_height,
                    });
                    run_start = None;
                }
                _ => {}
            }
        }
    }
    regions
}

fn coalesce_regions(mut regions: Vec<DamageRect>, limit: usize) -> Vec<DamageRect> {
    if regions.len() <= limit {
        return regions;
    }
    let first = regions.remove(0);
    let merged = regions.into_iter().fold(first, union);
    vec![merged]
}

fn union(a: DamageRect, b: DamageRect) -> DamageRect {
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    DamageRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

pub fn changed_area(regions: &[DamageRect]) -> u64 {
    regions
        .iter()
        .map(|region| u64::from(region.width) * u64::from(region.height))
        .sum()
}

pub fn encode_region_update(
    frame: &VideoFrame,
    regions: &[DamageRect],
    sequence: u64,
) -> Result<WireMessage> {
    if frame.layout().pixel_format() != PixelFormat::Bgra32 {
        return Err(DcError::Unsupported(
            "region encoding currently requires BGRA32 input".into(),
        ));
    }
    if regions.is_empty() || regions.len() > MAX_DESKTOP_REGIONS {
        return Err(DcError::InvalidInput(
            "region update has an invalid region count".into(),
        ));
    }
    let layout = frame.layout();
    let mut raw = Vec::new();
    let mut wire_regions = Vec::with_capacity(regions.len());
    for region in regions {
        validate_region(*region, layout.size().width(), layout.size().height())?;
        for row in 0..region.height {
            let start = (region.y + row) as usize * layout.stride() + region.x as usize * 4;
            let end = start + region.width as usize * 4;
            raw.extend_from_slice(&frame.data()[start..end]);
        }
        wire_regions.push(DesktopRect {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        });
    }
    let compressed = pack_bits(&raw);
    let (encoding, data) = if compressed.len() < raw.len() {
        (REGION_ENCODING_PACK_BITS, compressed)
    } else {
        (REGION_ENCODING_RAW, raw)
    };
    Ok(WireMessage::DesktopUpdate {
        sequence,
        timestamp_millis: u64::try_from(frame.timestamp().as_millis())
            .map_err(|_| DcError::InvalidInput("frame timestamp exceeds protocol range".into()))?,
        desktop_width: layout.size().width(),
        desktop_height: layout.size().height(),
        encoding,
        regions: wire_regions,
        data,
    })
}

pub fn decode_region_payload(message: &WireMessage) -> Result<Vec<u8>> {
    let WireMessage::DesktopUpdate {
        desktop_width,
        desktop_height,
        encoding,
        regions,
        data,
        ..
    } = message
    else {
        return Err(DcError::InvalidInput("expected desktop update".into()));
    };
    let expected = regions.iter().try_fold(0_usize, |total, region| {
        validate_wire_region(*region, *desktop_width, *desktop_height)?;
        let bytes = region
            .width
            .checked_mul(region.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| DcError::Codec("desktop region size overflowed".into()))?;
        total
            .checked_add(bytes)
            .ok_or_else(|| DcError::Codec("desktop update size overflowed".into()))
    })?;
    match *encoding {
        REGION_ENCODING_RAW if data.len() == expected => Ok(data.clone()),
        REGION_ENCODING_RAW => Err(DcError::Codec(
            "raw desktop update has the wrong payload size".into(),
        )),
        REGION_ENCODING_PACK_BITS => unpack_bits(data, expected),
        _ => Err(DcError::Codec("unknown desktop region encoding".into())),
    }
}

fn validate_region(region: DamageRect, width: u32, height: u32) -> Result<()> {
    validate_wire_region(
        DesktopRect {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        },
        width,
        height,
    )
}

fn validate_wire_region(region: DesktopRect, width: u32, height: u32) -> Result<()> {
    if region.width == 0
        || region.height == 0
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|right| right > width)
        || region
            .y
            .checked_add(region.height)
            .is_none_or(|bottom| bottom > height)
    {
        return Err(DcError::Codec(
            "desktop update region is outside the frame".into(),
        ));
    }
    Ok(())
}

fn pack_bits(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let run = repeated_run(input, index);
        if run >= 4 {
            output.push(0x80 | (run as u8 - 1));
            output.push(input[index]);
            index += run;
            continue;
        }
        let literal_start = index;
        index += run.max(1);
        while index < input.len() && index - literal_start < 128 && repeated_run(input, index) < 4 {
            index += repeated_run(input, index).max(1);
        }
        index = index.min(literal_start + 128);
        output.push((index - literal_start - 1) as u8);
        output.extend_from_slice(&input[literal_start..index]);
    }
    output
}

fn repeated_run(input: &[u8], start: usize) -> usize {
    let Some(value) = input.get(start) else {
        return 0;
    };
    input[start..]
        .iter()
        .take(128)
        .take_while(|candidate| *candidate == value)
        .count()
}

fn unpack_bits(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(expected);
    let mut index = 0;
    while index < input.len() && output.len() < expected {
        let header = input[index];
        index += 1;
        let count = usize::from(header & 0x7f) + 1;
        if header & 0x80 != 0 {
            let value = *input
                .get(index)
                .ok_or_else(|| DcError::Codec("truncated region repeat run".into()))?;
            index += 1;
            output.resize(
                output
                    .len()
                    .checked_add(count)
                    .ok_or_else(|| DcError::Codec("region payload overflowed".into()))?,
                value,
            );
        } else {
            let end = index
                .checked_add(count)
                .ok_or_else(|| DcError::Codec("region payload overflowed".into()))?;
            let bytes = input
                .get(index..end)
                .ok_or_else(|| DcError::Codec("truncated region literal run".into()))?;
            output.extend_from_slice(bytes);
            index = end;
        }
        if output.len() > expected {
            return Err(DcError::Codec(
                "region payload exceeds expected size".into(),
            ));
        }
    }
    if index != input.len() || output.len() != expected {
        return Err(DcError::Codec("region payload has the wrong size".into()));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_media::{FrameLayout, FrameSize};
    use std::time::Duration;

    fn frame(data: Vec<u8>) -> VideoFrame {
        let size = FrameSize::new(128, 64).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        VideoFrame::new(0, Duration::ZERO, layout, data).unwrap()
    }

    #[test]
    fn detector_reports_only_changed_tiles_after_first_frame() {
        let mut detector = DirtyRegionDetector::default();
        let first = frame(vec![0; 128 * 64 * 4]);
        assert_eq!(detector.detect(&first).unwrap()[0].width, 128);
        let mut data = first.data().to_vec();
        data[(10 * 128 + 70) * 4] = 1;
        let regions = detector.detect(&frame(data)).unwrap();
        assert_eq!(
            regions,
            vec![DamageRect {
                x: 64,
                y: 0,
                width: 64,
                height: 64
            }]
        );
    }

    #[test]
    fn region_encoding_round_trips() {
        let frame = frame(vec![0xff; 128 * 64 * 4]);
        let message = encode_region_update(
            &frame,
            &[DamageRect {
                x: 4,
                y: 2,
                width: 8,
                height: 3,
            }],
            7,
        )
        .unwrap();
        assert_eq!(
            decode_region_payload(&message).unwrap(),
            vec![0xff; 8 * 3 * 4]
        );
    }

    #[test]
    fn pixel_diff_recovers_when_source_metadata_is_empty() {
        let mut detector = DirtyRegionDetector::default();
        let size = FrameSize::new(128, 64).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        let first = VideoFrame::new(0, Duration::ZERO, layout, vec![0; 128 * 64 * 4])
            .unwrap()
            .with_metadata(dc_media::FrameMetadata {
                damage: Some(Vec::new()),
                cursor: None,
            })
            .unwrap();
        detector.detect(&first).unwrap();
        let mut data = first.data().to_vec();
        data[(10 * 128 + 70) * 4] = 1;
        let second = VideoFrame::new(1, Duration::from_millis(1), layout, data)
            .unwrap()
            .with_metadata(dc_media::FrameMetadata {
                damage: Some(Vec::new()),
                cursor: None,
            })
            .unwrap();
        assert_eq!(
            detector.detect(&second).unwrap(),
            vec![DamageRect {
                x: 64,
                y: 0,
                width: 64,
                height: 64,
            }]
        );
    }
}

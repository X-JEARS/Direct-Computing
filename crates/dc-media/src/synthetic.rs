use crate::{FrameLayout, FrameSink, FrameSize, FrameSource, PixelFormat, VideoFrame};
use dc_common::Result;
use std::time::Duration;

pub struct SyntheticFrameSource {
    layout: FrameLayout,
    next_sequence: u64,
    next_timestamp: Duration,
    frame_interval: Duration,
}

impl SyntheticFrameSource {
    pub fn new(size: FrameSize, frames_per_second: u32) -> Result<Self> {
        if frames_per_second == 0 {
            return Err(dc_common::DcError::InvalidInput(
                "synthetic frame rate must be non-zero".into(),
            ));
        }
        Ok(Self {
            layout: FrameLayout::packed(size, PixelFormat::Bgra32)?,
            next_sequence: 0,
            next_timestamp: Duration::ZERO,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(frames_per_second)),
        })
    }
}

impl FrameSource for SyntheticFrameSource {
    fn capture(&mut self) -> Result<VideoFrame> {
        let sequence = self.next_sequence;
        let timestamp = self.next_timestamp;
        let mut data = vec![0; self.layout.data_len()];
        let width = self.layout.size().width() as usize;
        let height = self.layout.size().height() as usize;

        for y in 0..height {
            for x in 0..width {
                let offset = y * self.layout.stride() + x * 4;
                data[offset] = (x as u64 + sequence).to_le_bytes()[0];
                data[offset + 1] = (y as u64 + sequence).to_le_bytes()[0];
                data[offset + 2] = sequence.wrapping_mul(7).to_le_bytes()[0];
                data[offset + 3] = u8::MAX;
            }
        }

        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.next_timestamp = self.next_timestamp.saturating_add(self.frame_interval);
        VideoFrame::new(sequence, timestamp, self.layout, data)
    }
}

#[derive(Debug, Default)]
pub struct ChecksumSink {
    frames_presented: u64,
    last_sequence: Option<u64>,
    last_checksum: Option<u64>,
}

impl ChecksumSink {
    pub const fn frames_presented(&self) -> u64 {
        self.frames_presented
    }

    pub const fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    pub const fn last_checksum(&self) -> Option<u64> {
        self.last_checksum
    }
}

impl FrameSink for ChecksumSink {
    fn present(&mut self, frame: VideoFrame) -> Result<()> {
        // FNV-1a makes frame changes observable without adding a hashing dependency.
        let checksum = frame
            .data()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        self.frames_presented += 1;
        self.last_sequence = Some(frame.sequence());
        self.last_checksum = Some(checksum);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LoopbackPipeline, RawVideoDecoder, RawVideoEncoder};

    #[test]
    fn synthetic_frames_change_over_time() {
        let size = FrameSize::new(4, 3).unwrap();
        let mut source = SyntheticFrameSource::new(size, 30).unwrap();
        let first = source.capture().unwrap();
        let second = source.capture().unwrap();
        assert_eq!(first.sequence(), 0);
        assert_eq!(second.sequence(), 1);
        assert_ne!(first.data(), second.data());
        assert!(second.timestamp() > first.timestamp());
    }

    #[test]
    fn synthetic_source_rejects_zero_frame_rate() {
        let size = FrameSize::new(4, 3).unwrap();
        assert!(SyntheticFrameSource::new(size, 0).is_err());
    }

    #[test]
    fn loopback_delivers_every_frame_without_data_loss() {
        let size = FrameSize::new(8, 6).unwrap();
        let source = SyntheticFrameSource::new(size, 30).unwrap();
        let mut pipeline = LoopbackPipeline::new(
            source,
            RawVideoEncoder,
            RawVideoDecoder,
            ChecksumSink::default(),
        );

        let stats = pipeline.run_frames(3).unwrap();
        assert_eq!(stats.frames_processed, 3);
        assert_eq!(stats.source_bytes, 8 * 6 * 4 * 3);
        assert_eq!(stats.encoded_bytes, stats.source_bytes);
        assert_eq!(pipeline.sink().frames_presented(), 3);
        assert_eq!(pipeline.sink().last_sequence(), Some(2));
        assert!(pipeline.sink().last_checksum().is_some());
    }
}

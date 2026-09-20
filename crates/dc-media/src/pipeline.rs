use crate::{EncodedVideoPacket, VideoCodec, VideoFrame};
use dc_common::{DcError, Result};
use std::time::{Duration, Instant};

pub trait FrameSource {
    fn capture(&mut self) -> Result<VideoFrame>;
}

pub trait VideoEncoder {
    fn codec(&self) -> VideoCodec;
    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedVideoPacket>;
}

pub trait VideoDecoder {
    fn codec(&self) -> VideoCodec;
    fn decode(&mut self, packet: EncodedVideoPacket) -> Result<VideoFrame>;
}

pub trait FrameSink {
    fn present(&mut self, frame: VideoFrame) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipelineStats {
    pub frames_processed: u64,
    pub source_bytes: u64,
    pub encoded_bytes: u64,
    pub capture_time: Duration,
    pub encode_time: Duration,
    pub decode_time: Duration,
    pub present_time: Duration,
}

impl PipelineStats {
    pub fn delta_since(self, earlier: Self) -> Self {
        Self {
            frames_processed: self
                .frames_processed
                .saturating_sub(earlier.frames_processed),
            source_bytes: self.source_bytes.saturating_sub(earlier.source_bytes),
            encoded_bytes: self.encoded_bytes.saturating_sub(earlier.encoded_bytes),
            capture_time: self.capture_time.saturating_sub(earlier.capture_time),
            encode_time: self.encode_time.saturating_sub(earlier.encode_time),
            decode_time: self.decode_time.saturating_sub(earlier.decode_time),
            present_time: self.present_time.saturating_sub(earlier.present_time),
        }
    }
}

pub struct LoopbackPipeline<S, E, D, K> {
    source: S,
    encoder: E,
    decoder: D,
    sink: K,
    stats: PipelineStats,
}

impl<S, E, D, K> LoopbackPipeline<S, E, D, K>
where
    S: FrameSource,
    E: VideoEncoder,
    D: VideoDecoder,
    K: FrameSink,
{
    pub fn new(source: S, encoder: E, decoder: D, sink: K) -> Self {
        Self {
            source,
            encoder,
            decoder,
            sink,
            stats: PipelineStats::default(),
        }
    }

    pub fn process_next_frame(&mut self) -> Result<PipelineStats> {
        let started = Instant::now();
        let frame = self.source.capture()?;
        self.stats.capture_time += started.elapsed();
        let source_bytes = frame.data().len() as u64;

        let started = Instant::now();
        let packet = self.encoder.encode(frame)?;
        self.stats.encode_time += started.elapsed();
        if packet.codec() != self.encoder.codec() {
            return Err(DcError::Codec(format!(
                "encoder declared {:?} but produced {:?}",
                self.encoder.codec(),
                packet.codec()
            )));
        }
        if packet.codec() != self.decoder.codec() {
            return Err(DcError::Codec(format!(
                "decoder expects {:?} but received {:?}",
                self.decoder.codec(),
                packet.codec()
            )));
        }
        let encoded_bytes = packet.data().len() as u64;

        let started = Instant::now();
        let decoded = self.decoder.decode(packet)?;
        self.stats.decode_time += started.elapsed();

        let started = Instant::now();
        self.sink.present(decoded)?;
        self.stats.present_time += started.elapsed();

        self.stats.frames_processed += 1;
        self.stats.source_bytes += source_bytes;
        self.stats.encoded_bytes += encoded_bytes;
        Ok(self.stats)
    }

    pub fn run_frames(&mut self, count: u64) -> Result<PipelineStats> {
        for _ in 0..count {
            self.process_next_frame()?;
        }
        Ok(self.stats)
    }

    pub const fn stats(&self) -> PipelineStats {
        self.stats
    }

    pub fn sink(&self) -> &K {
        &self.sink
    }

    pub fn sink_mut(&mut self) -> &mut K {
        &mut self.sink
    }

    pub fn into_parts(self) -> (S, E, D, K) {
        (self.source, self.encoder, self.decoder, self.sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_delta_subtracts_each_measurement() {
        let current = PipelineStats {
            frames_processed: 10,
            source_bytes: 500,
            encoded_bytes: 50,
            capture_time: Duration::from_millis(40),
            encode_time: Duration::from_millis(80),
            decode_time: Duration::from_millis(20),
            present_time: Duration::from_millis(10),
        };
        let earlier = PipelineStats {
            frames_processed: 4,
            source_bytes: 200,
            encoded_bytes: 20,
            capture_time: Duration::from_millis(10),
            encode_time: Duration::from_millis(30),
            decode_time: Duration::from_millis(5),
            present_time: Duration::from_millis(3),
        };
        let delta = current.delta_since(earlier);
        assert_eq!(delta.frames_processed, 6);
        assert_eq!(delta.source_bytes, 300);
        assert_eq!(delta.encoded_bytes, 30);
        assert_eq!(delta.capture_time, Duration::from_millis(30));
        assert_eq!(delta.encode_time, Duration::from_millis(50));
        assert_eq!(delta.decode_time, Duration::from_millis(15));
        assert_eq!(delta.present_time, Duration::from_millis(7));
    }
}

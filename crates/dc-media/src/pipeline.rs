use crate::{EncodedVideoPacket, VideoCodec, VideoFrame};
use dc_common::{DcError, Result};

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
        let frame = self.source.capture()?;
        let source_bytes = frame.data().len() as u64;
        let packet = self.encoder.encode(frame)?;
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
        let decoded = self.decoder.decode(packet)?;
        self.sink.present(decoded)?;

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

    pub fn into_parts(self) -> (S, E, D, K) {
        (self.source, self.encoder, self.decoder, self.sink)
    }
}

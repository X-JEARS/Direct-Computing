use crate::{EncodedVideoPacket, VideoCodec, VideoDecoder, VideoEncoder, VideoFrame};
use dc_common::{DcError, Result};

#[derive(Debug, Default)]
pub struct RawVideoEncoder;

impl VideoEncoder for RawVideoEncoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Raw
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedVideoPacket> {
        EncodedVideoPacket::from_raw_frame(frame)
    }
}

#[derive(Debug, Default)]
pub struct RawVideoDecoder;

impl VideoDecoder for RawVideoDecoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Raw
    }

    fn decode(&mut self, packet: EncodedVideoPacket) -> Result<VideoFrame> {
        if packet.codec() != VideoCodec::Raw {
            return Err(DcError::InvalidInput(format!(
                "raw decoder received {:?} data",
                packet.codec()
            )));
        }
        let sequence = packet.sequence();
        let timestamp = packet.timestamp();
        let layout = packet.source_layout();
        VideoFrame::new(sequence, timestamp, layout, packet.into_data())
    }
}

use crate::{FrameLayout, VideoFrame};
use dc_common::{DcError, Result};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    Raw,
    H264,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedVideoPacket {
    codec: VideoCodec,
    sequence: u64,
    timestamp: Duration,
    keyframe: bool,
    source_layout: FrameLayout,
    data: Vec<u8>,
}

impl EncodedVideoPacket {
    pub fn new(
        codec: VideoCodec,
        sequence: u64,
        timestamp: Duration,
        keyframe: bool,
        source_layout: FrameLayout,
        data: Vec<u8>,
    ) -> Result<Self> {
        if data.is_empty() {
            return Err(DcError::InvalidInput(
                "encoded video packet cannot be empty".into(),
            ));
        }
        Ok(Self {
            codec,
            sequence,
            timestamp,
            keyframe,
            source_layout,
            data,
        })
    }

    pub fn from_raw_frame(frame: VideoFrame) -> Result<Self> {
        let sequence = frame.sequence();
        let timestamp = frame.timestamp();
        let source_layout = frame.layout();
        Self::new(
            VideoCodec::Raw,
            sequence,
            timestamp,
            true,
            source_layout,
            frame.into_data(),
        )
    }

    pub const fn codec(&self) -> VideoCodec {
        self.codec
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn timestamp(&self) -> Duration {
        self.timestamp
    }

    pub const fn is_keyframe(&self) -> bool {
        self.keyframe
    }

    pub const fn source_layout(&self) -> FrameLayout {
        self.source_layout
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Replace the transport sequence without changing the encoded payload.
    ///
    /// Capture sequence numbers are allowed to contain gaps when an encoder
    /// skips a frame.  The transport must instead expose a sequence for each
    /// packet that was actually emitted, so the viewer can distinguish an
    /// encoder skip from a lost packet.
    pub fn with_sequence(self, sequence: u64) -> Self {
        Self { sequence, ..self }
    }
}

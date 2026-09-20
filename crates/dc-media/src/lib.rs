//! Media data models and local processing pipelines.

mod frame;
#[cfg(feature = "openh264")]
mod h264;
mod output;
mod packet;
mod pipeline;
mod raw;
mod synthetic;

pub use frame::{FrameLayout, FrameSize, PixelFormat, VideoFrame};
#[cfg(feature = "openh264")]
pub use h264::{OpenH264Decoder, OpenH264Encoder};
pub use output::{write_bmp, LastFrameSink};
pub use packet::{EncodedVideoPacket, VideoCodec};
pub use pipeline::{
    FrameSink, FrameSource, LoopbackPipeline, PipelineStats, VideoDecoder, VideoEncoder,
};
pub use raw::{RawVideoDecoder, RawVideoEncoder};
pub use synthetic::{ChecksumSink, SyntheticFrameSource};

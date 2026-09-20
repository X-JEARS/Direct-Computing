use dc_common::{init_logging, log, DcError, LogLevel, Result};
use dc_media::{
    ChecksumSink, FrameSize, LoopbackPipeline, OpenH264Decoder, OpenH264Encoder,
    SyntheticFrameSource,
};
use dc_protocol::PROTOCOL_VERSION;

fn main() {
    init_logging();
    if let Err(error) = run(std::env::args().skip(1)) {
        log(LogLevel::Error, "direct-computing", &error.to_string());
        std::process::exit(2);
    }
}

fn run(mut arguments: impl Iterator<Item = String>) -> Result<()> {
    match arguments.next().as_deref() {
        None => {
            log(
                LogLevel::Info,
                "direct-computing",
                &format!(
                    "Direct Computing bootstrap (protocol {}.{})",
                    PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
                ),
            );
            Ok(())
        }
        Some("--loopback") => {
            let frame_count = arguments
                .next()
                .map(|value| {
                    value.parse::<u64>().map_err(|_| {
                        DcError::InvalidInput(format!("invalid loopback frame count: {value}"))
                    })
                })
                .transpose()?
                .unwrap_or(30);
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(
                    "usage: direct-computing [--loopback [frame-count]]".into(),
                ));
            }
            run_loopback(frame_count)
        }
        Some(argument) => Err(DcError::InvalidInput(format!(
            "unknown argument {argument}; usage: direct-computing [--loopback [frame-count]]"
        ))),
    }
}

fn run_loopback(frame_count: u64) -> Result<()> {
    let size = FrameSize::new(320, 180)?;
    let source = SyntheticFrameSource::new(size, 30)?;
    let mut pipeline = LoopbackPipeline::new(
        source,
        OpenH264Encoder::new(2_000_000, 30.0)?,
        OpenH264Decoder::new()?,
        ChecksumSink::default(),
    );
    let stats = pipeline.run_frames(frame_count)?;
    let checksum = pipeline.sink().last_checksum().unwrap_or_default();
    log(
        LogLevel::Info,
        "direct-computing::media",
        &format!(
            "loopback complete: frames={}, source_bytes={}, encoded_bytes={}, last_checksum={checksum:016x}",
            stats.frames_processed, stats.source_bytes, stats.encoded_bytes
        ),
    );
    Ok(())
}

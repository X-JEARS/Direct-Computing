use dc_common::{init_logging, log, DcError, LogLevel, Result};
#[cfg(target_os = "windows")]
use dc_media::{write_bmp, LastFrameSink};
use dc_media::{
    ChecksumSink, FrameSize, LoopbackPipeline, OpenH264Decoder, OpenH264Encoder,
    SyntheticFrameSource,
};
#[cfg(target_os = "windows")]
use dc_platform::WindowsDesktopCapturer;
use dc_protocol::PROTOCOL_VERSION;
use std::path::Path;

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
            let frame_count = parse_optional(&mut arguments, "loopback frame count", 30_u64)?;
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(usage().into()));
            }
            run_loopback(frame_count)
        }
        Some("--capture-test") => {
            let frame_count = parse_optional(&mut arguments, "capture frame count", 30_u64)?;
            let output_index = parse_optional(&mut arguments, "display index", 0_u32)?;
            let output_path = arguments
                .next()
                .unwrap_or_else(|| "dc-capture-test.bmp".into());
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(usage().into()));
            }
            if frame_count == 0 {
                return Err(DcError::InvalidInput(
                    "capture frame count must be non-zero".into(),
                ));
            }
            run_capture_test(frame_count, output_index, Path::new(&output_path))
        }
        Some(argument) => Err(DcError::InvalidInput(format!(
            "unknown argument {argument}; {}",
            usage()
        ))),
    }
}

fn parse_optional<T>(
    arguments: &mut impl Iterator<Item = String>,
    label: &str,
    default: T,
) -> Result<T>
where
    T: std::str::FromStr,
{
    arguments
        .next()
        .map(|value| {
            value
                .parse::<T>()
                .map_err(|_| DcError::InvalidInput(format!("invalid {label}: {value}")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

const fn usage() -> &'static str {
    "usage: direct-computing [--loopback [frame-count] | --capture-test [frame-count] [display-index] [output.bmp]]"
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

#[cfg(target_os = "windows")]
fn run_capture_test(frame_count: u64, output_index: u32, output_path: &Path) -> Result<()> {
    let source = WindowsDesktopCapturer::new(output_index, 1_000)?;
    let mut pipeline = LoopbackPipeline::new(
        source,
        OpenH264Encoder::new(20_000_000, 30.0)?,
        OpenH264Decoder::new()?,
        LastFrameSink::default(),
    );
    let stats = pipeline.run_frames(frame_count)?;
    let (source, _, _, sink) = pipeline.into_parts();
    let frame = sink.into_last_frame().ok_or_else(|| {
        DcError::Platform("capture pipeline completed without a decoded frame".into())
    })?;
    write_bmp(output_path, &frame)?;
    log(
        LogLevel::Info,
        "direct-computing::windows",
        &format!(
            "capture complete: display={} ({}), frames={}, source_bytes={}, encoded_bytes={}, output={}",
            source.output_index(),
            source.output_name(),
            stats.frames_processed,
            stats.source_bytes,
            stats.encoded_bytes,
            output_path.display()
        ),
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_capture_test(_frame_count: u64, _output_index: u32, _output_path: &Path) -> Result<()> {
    Err(DcError::Unsupported(
        "--capture-test currently requires Windows".into(),
    ))
}

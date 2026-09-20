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
use dc_ui::PreviewWindowSink;
use std::path::Path;
use std::time::{Duration, Instant};

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
        Some("--window-test") => {
            let duration_seconds = parse_optional(&mut arguments, "duration", 10_u64)?;
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(usage().into()));
            }
            run_window_test(duration_seconds)
        }
        Some("--preview") => {
            let output_index = parse_optional(&mut arguments, "display index", 0_u32)?;
            let duration_seconds = parse_optional(&mut arguments, "duration", 0_u64)?;
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(usage().into()));
            }
            run_preview(output_index, duration_seconds)
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
    "usage: direct-computing [--loopback [frame-count] | --window-test [duration-seconds] | --capture-test [frame-count] [display-index] [output.bmp] | --preview [display-index] [duration-seconds]]"
}

fn run_window_test(duration_seconds: u64) -> Result<()> {
    let size = FrameSize::new(640, 360)?;
    let source = SyntheticFrameSource::new(size, 30)?;
    let pipeline = LoopbackPipeline::new(
        source,
        OpenH264Encoder::new(2_000_000, 30.0)?,
        OpenH264Decoder::new()?,
        PreviewWindowSink::new("Direct Computing - Synthetic Preview"),
    );
    run_preview_loop(pipeline, duration_seconds)
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

#[cfg(target_os = "windows")]
fn run_preview(output_index: u32, duration_seconds: u64) -> Result<()> {
    let source = WindowsDesktopCapturer::new(output_index, 100)?;
    let pipeline = LoopbackPipeline::new(
        source,
        OpenH264Encoder::new(20_000_000, 30.0)?,
        OpenH264Decoder::new()?,
        PreviewWindowSink::new("Direct Computing - Desktop Preview"),
    );
    run_preview_loop(pipeline, duration_seconds)
}

#[cfg(not(target_os = "windows"))]
fn run_preview(_output_index: u32, _duration_seconds: u64) -> Result<()> {
    Err(DcError::Unsupported(
        "--preview currently requires Windows; use --window-test to test the window".into(),
    ))
}

fn run_preview_loop<S>(
    mut pipeline: LoopbackPipeline<S, OpenH264Encoder, OpenH264Decoder, PreviewWindowSink>,
    duration_seconds: u64,
) -> Result<()>
where
    S: dc_media::FrameSource,
{
    let started_at = Instant::now();
    let deadline =
        (duration_seconds != 0).then(|| started_at + Duration::from_secs(duration_seconds));
    let frame_interval = Duration::from_secs_f64(1.0 / 30.0);
    let mut next_frame_at = started_at;
    let mut report_started_at = started_at;
    let mut previous_stats = pipeline.stats();

    while pipeline.sink().is_open() && deadline.is_none_or(|value| Instant::now() < value) {
        let frames_before = pipeline.stats().frames_processed;
        match pipeline.process_next_frame() {
            Ok(stats) if stats.frames_processed == frames_before => {
                pipeline.sink_mut().pump_events();
            }
            Ok(_) => {}
            Err(DcError::Timeout(_)) => pipeline.sink_mut().pump_events(),
            Err(error) => return Err(error),
        }

        next_frame_at += frame_interval;
        let now = Instant::now();
        if now < next_frame_at {
            std::thread::sleep(next_frame_at - now);
        } else {
            next_frame_at = now;
        }

        let now = Instant::now();
        let report_elapsed = now.duration_since(report_started_at);
        if report_elapsed >= Duration::from_secs(1) {
            let current_stats = pipeline.stats();
            let interval = current_stats.delta_since(previous_stats);
            let status = format_stats(interval, report_elapsed);
            pipeline.sink_mut().set_status(&status);
            log(LogLevel::Info, "direct-computing::preview", &status);
            previous_stats = current_stats;
            report_started_at = now;
        }
    }

    let elapsed = started_at.elapsed();
    let status = format_stats(pipeline.stats(), elapsed);
    log(
        LogLevel::Info,
        "direct-computing::preview",
        &format!("preview complete: {status}, elapsed={elapsed:.2?}"),
    );
    Ok(())
}

fn format_stats(stats: dc_media::PipelineStats, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    let frames = stats.frames_processed;
    let captured_frames = frames.saturating_add(stats.frames_skipped);
    format!(
        "{:.1} FPS | skipped {} | raw {:.1} MB/s | H.264 {:.1} KB/s | capture {:.1} ms | encode {:.1} ms | decode {:.1} ms | display {:.1} ms",
        frames as f64 / seconds,
        stats.frames_skipped,
        stats.source_bytes as f64 / seconds / 1_000_000.0,
        stats.encoded_bytes as f64 / seconds / 1_000.0,
        average_millis(stats.capture_time, captured_frames),
        average_millis(stats.encode_time, captured_frames),
        average_millis(stats.decode_time, frames),
        average_millis(stats.present_time, frames),
    )
}

fn average_millis(duration: Duration, samples: u64) -> f64 {
    if samples == 0 {
        0.0
    } else {
        duration.as_secs_f64() * 1_000.0 / samples as f64
    }
}

use dc_auth::{PasswordVerifier, Permissions};
use dc_common::{init_logging, log, DcError, LogLevel, Result};
use dc_desktop::validate_input;
use dc_desktop::{
    changed_area, classify_video_datagram, decode_region_payload, encode_region_update,
    packet_to_message, packetize_h264_nal_message, packetize_video_message, DirtyRegionDetector,
    H264NalDatagramReassembler, VideoDatagramKind, VideoDatagramReassembler,
};
#[cfg(target_os = "windows")]
use dc_media::{write_bmp, LastFrameSink};
use dc_media::{
    ChecksumSink, FrameSize, LoopbackPipeline, OpenH264Decoder, OpenH264Encoder, PixelFormat,
    SyntheticFrameSource, VideoEncoder, VideoFrame,
};
#[cfg(target_os = "macos")]
use dc_platform::VideoToolboxH264Decoder;
#[cfg(all(target_os = "windows", feature = "x264"))]
use dc_platform::X264Encoder;
#[cfg(target_os = "windows")]
use dc_platform::{InputInjector, WindowsInputInjector};
#[cfg(target_os = "windows")]
use dc_platform::{
    WindowsDesktopCapturer, WindowsMediaFoundationH264Decoder, WindowsMediaFoundationH264Encoder,
};
use dc_protocol::PROTOCOL_VERSION;
use dc_protocol::{Capabilities, WireMessage};
use dc_session::{authenticate_client, authenticate_server};
use dc_transport::{format_fingerprint, parse_fingerprint, QuicClient, QuicConnection, QuicServer};
use dc_ui::PreviewWindowSink;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// The encoder's bitrate is an average target; keyframes can still be larger.
#[derive(Clone, Copy, Debug)]
struct RateProfile {
    name: &'static str,
    initial_bitrate: u32,
    initial_fps: u32,
    min_bitrate: u32,
    min_fps: u32,
    max_bitrate: u32,
    max_fps: u32,
    color_depth_bits: u8,
    color_masks: [u8; 3],
}

const DEFAULT_RATE_PROFILE: RateProfile = RateProfile {
    name: "balanced",
    initial_bitrate: 1_000_000,
    initial_fps: 12,
    min_bitrate: 384_000,
    min_fps: 6,
    max_bitrate: 4_000_000,
    max_fps: 30,
    color_depth_bits: 16,
    // BGRA storage: five blue bits, six green bits, five red bits.
    color_masks: [0xF8, 0xFC, 0xF8],
};

const ULTRA_LOW_RATE_PROFILE: RateProfile = RateProfile {
    name: "ultra-low",
    initial_bitrate: 64_000,
    initial_fps: 3,
    min_bitrate: 64_000,
    min_fps: 3,
    max_bitrate: 160_000,
    max_fps: 5,
    color_depth_bits: 6,
    color_masks: [0xC0, 0xC0, 0xC0],
};
const NETWORK_MAX_BITRATE: u32 = 4_000_000;
const NETWORK_MAX_FPS: u32 = 30;
const RATE_UPGRADE_INTERVAL: Duration = Duration::from_secs(5);
const RATE_UPGRADE_MIN_INTER_FRAMES: u32 = 8;
// At 1440x900, even a low-bitrate H.264 inter frame can occupy 60-90 MTU
// fragments.  A cap of 32 rejected nearly every changed desktop frame and
// forced another large reliable keyframe, which is disastrous on a narrow
// link.  QUIC still bounds the datagram queue and the Viewer discards an
// incomplete/stale frame, so allowing up to 96 fragments is latency-oriented
// rather than a reliable-frame guarantee.
const MAX_DATAGRAM_FRAGMENTS_PER_FRAME: usize = 96;
const STARTUP_ZERO_FRAME_GRACE: Duration = Duration::from_secs(2);
const NETWORK_CHROMA_SUBSAMPLING: &str = "4:2:0";
const MEDIA_PROTOCOL_REVISION: &str = "hybrid-v4-h264-nal-regions";
const FULL_FRAME_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const REGION_UPDATE_MAX_AREA_PERCENT: u64 = 35;
const REGION_UPDATE_MAX_BYTES: usize = 64 * 1024;

struct StreamControl {
    profile: RateProfile,
    keyframe_requested: AtomicBool,
    recovery_pending: AtomicBool,
    shutdown: AtomicBool,
    target_bitrate: AtomicU32,
    target_fps: AtomicU32,
}

impl StreamControl {
    const fn new(profile: RateProfile) -> Self {
        Self {
            profile,
            keyframe_requested: AtomicBool::new(false),
            recovery_pending: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            target_bitrate: AtomicU32::new(profile.initial_bitrate),
            target_fps: AtomicU32::new(profile.initial_fps),
        }
    }

    fn update_rate(&self, bitrate: u32, frames_per_second: u16) {
        self.target_bitrate.store(
            bitrate.clamp(
                self.profile.min_bitrate,
                self.profile.max_bitrate.min(NETWORK_MAX_BITRATE),
            ),
            Ordering::Release,
        );
        self.target_fps.store(
            u32::from(frames_per_second).clamp(
                self.profile.min_fps,
                self.profile.max_fps.min(NETWORK_MAX_FPS),
            ),
            Ordering::Release,
        );
    }

    fn reduce_rate(&self) -> (u32, u16) {
        let current_bitrate = self.target_bitrate.load(Ordering::Acquire);
        let current_fps = self.target_fps.load(Ordering::Acquire) as u16;
        let (bitrate, fps) = reduce_stream_rate(
            current_bitrate,
            current_fps,
            self.profile.min_bitrate,
            self.profile.min_fps,
        );
        self.update_rate(bitrate, fps);
        (bitrate, fps)
    }

    fn request_keyframe(&self) -> bool {
        if self
            .recovery_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.keyframe_requested.store(true, Ordering::Release);
        true
    }

    fn request_lower_rate_keyframe(&self) -> Option<(u32, u16)> {
        if self
            .recovery_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let previous_bitrate = self.target_bitrate.load(Ordering::Acquire);
        let previous_fps = self.target_fps.load(Ordering::Acquire);
        let rate = self.reduce_rate();
        if rate.0 == previous_bitrate && rate.1 as u32 == previous_fps {
            // The profile is already at its floor. Re-requesting a reliable
            // keyframe for every oversized inter frame would create an
            // endless keyframe/recovery loop without changing encoder output.
            self.recovery_pending.store(false, Ordering::Release);
            return None;
        }
        self.keyframe_requested.store(true, Ordering::Release);
        Some(rate)
    }

    fn acknowledge_keyframe(&self) {
        self.recovery_pending.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
struct EncodedFrame {
    message: Arc<WireMessage>,
    sequence: u64,
    encoded_bytes: u64,
    capture_time: Duration,
    encode_time: Duration,
    reliable_barrier: bool,
}

#[derive(Clone)]
struct ReceivedFrame {
    message: Arc<WireMessage>,
    received_at: Instant,
}

#[derive(Clone, Copy)]
struct MediaFeatures {
    hybrid_video: bool,
    desktop_optimizations: bool,
    h264_nal_datagrams: bool,
}

fn main() {
    init_logging();
    if let Err(error) = run(std::env::args().skip(1)) {
        log(LogLevel::Error, "direct-computing", &error.to_string());
        std::process::exit(2);
    }
}

fn run(arguments: impl Iterator<Item = String>) -> Result<()> {
    let mut arguments = arguments.collect::<Vec<_>>();
    let mut profile = DEFAULT_RATE_PROFILE;
    let mut use_x264 = false;
    let mut show_dirty_regions = false;
    while let Some(option) = arguments.first().map(String::as_str) {
        match option {
            "--ultra-low" => {
                profile = ULTRA_LOW_RATE_PROFILE;
                arguments.remove(0);
            }
            "--x264" => {
                use_x264 = true;
                arguments.remove(0);
            }
            "--show-dirty-regions" => {
                show_dirty_regions = true;
                arguments.remove(0);
            }
            _ => break,
        }
    }
    let mut arguments = arguments.into_iter();
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
        Some("--host") => {
            let first = arguments
                .next()
                .ok_or_else(|| DcError::InvalidInput("--host requires a password".into()))?;
            let (address, password) = match arguments.next() {
                Some(password) => (first, password),
                None => ("0.0.0.0:22100".to_owned(), first),
            };
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(usage().into()));
            }
            run_network_host(&address, &password, profile, use_x264)
        }
        Some("--connect") => {
            let address = arguments
                .next()
                .ok_or_else(|| DcError::InvalidInput("--connect requires an address".into()))?;
            let password = arguments
                .next()
                .ok_or_else(|| DcError::InvalidInput("--connect requires a password".into()))?;
            let fingerprint = arguments.next();
            if arguments.next().is_some() {
                return Err(DcError::InvalidInput(usage().into()));
            }
            run_network_viewer(
                &address,
                &password,
                fingerprint.as_deref(),
                profile,
                show_dirty_regions,
            )
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
    "usage: direct-computing [--ultra-low] [--x264] [--show-dirty-regions] [--host [addr] <password> | --connect <addr> <password> [cert-sha256] | --loopback [frame-count] | --window-test [duration-seconds] | --capture-test [frame-count] [display-index] [output.bmp] | --preview [display-index] [duration-seconds]]"
}

fn run_network_host(
    address: &str,
    password: &str,
    profile: RateProfile,
    use_x264: bool,
) -> Result<()> {
    let address = address
        .parse()
        .map_err(|_| DcError::InvalidInput(format!("invalid host address: {address}")))?;
    let verifier = PasswordVerifier::from_password(password)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| DcError::Platform(error.to_string()))?
        .block_on(async move {
            let server = QuicServer::bind(address)?;
            log(
                LogLevel::Info,
                "direct-computing::host",
                &format!(
                    "listening on {}; certificate sha256={}",
                    server.local_addr()?,
                    format_fingerprint(server.certificate_fingerprint())
                ),
            );
            let connection = server.accept().await?;
            let mut control = connection.accept_stream().await?;
            let permissions = Permissions {
                view_desktop: true,
                control_input: true,
                ..Permissions::empty()
            };
            let session = authenticate_server(
                &mut control,
                &verifier,
                permissions,
                Capabilities {
                    desktop: true,
                    control_input: true,
                    hybrid_video: true,
                    desktop_optimizations: true,
                    h264_nal_datagrams: true,
                    ..Capabilities::default()
                },
            )
            .await?;
            let hybrid_video = session.peer_capabilities.hybrid_video;
            let desktop_optimizations =
                hybrid_video && session.peer_capabilities.desktop_optimizations;
            let h264_nal_datagrams =
                hybrid_video && session.peer_capabilities.h264_nal_datagrams;
            log(
                LogLevel::Info,
                "direct-computing::host",
                &format!(
                    "media protocol={} peer hybrid_video={} (client build must support reliable keyframes)",
                    MEDIA_PROTOCOL_REVISION, hybrid_video
                ),
            );
            control.send(&WireMessage::OpenDesktop).await?;
            let stream_control = Arc::new(StreamControl::new(profile));
            let input_stream_control = stream_control.clone();
            tokio::spawn(async move {
                let result = receive_host_input(&mut control, input_stream_control.clone()).await;
                input_stream_control
                    .shutdown
                    .store(true, Ordering::Release);
                match result {
                    Ok(()) => log(
                        LogLevel::Info,
                        "direct-computing::host",
                        "client requested stream shutdown",
                    ),
                    Err(error) => log(
                        LogLevel::Warn,
                        "direct-computing::host",
                        &format!("input stream closed: {error}"),
                    ),
                }
            });
            let result = send_desktop_frames(
                &connection,
                stream_control,
                hybrid_video,
                desktop_optimizations,
                h264_nal_datagrams,
                profile,
                use_x264,
            )
            .await;
            if let Err(error) = &result {
                log(
                    LogLevel::Error,
                    "direct-computing::stream-host",
                    &format!("video stream stopped: {error}"),
                );
                connection.close(1, b"host video stream stopped");
            }
            result
        })
}

#[cfg(target_os = "windows")]
async fn receive_host_input(
    stream: &mut dc_transport::FramedStream,
    stream_control: Arc<StreamControl>,
) -> Result<()> {
    let mut injector = WindowsInputInjector::new();
    loop {
        match stream.receive().await? {
            WireMessage::Input(event) => {
                injector.inject(&event)?;
            }
            WireMessage::KeyframeRequest { .. } => {
                stream_control.request_keyframe();
            }
            WireMessage::KeyframeAck { sequence } => {
                stream_control.acknowledge_keyframe();
                log(
                    LogLevel::Debug,
                    "direct-computing::stream-host",
                    &format!("viewer acknowledged keyframe sequence={sequence}; resuming capture"),
                );
            }
            WireMessage::RateHint {
                bitrate,
                frames_per_second,
            } => {
                stream_control.update_rate(bitrate, frames_per_second);
            }
            WireMessage::Close => {
                stream_control.shutdown.store(true, Ordering::Release);
                return Ok(());
            }
            _ => {}
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn receive_host_input(
    stream: &mut dc_transport::FramedStream,
    stream_control: Arc<StreamControl>,
) -> Result<()> {
    loop {
        match stream.receive().await? {
            WireMessage::Input(_) => {
                return Err(DcError::Unsupported(
                    "remote input injection requires Windows Host".into(),
                ));
            }
            WireMessage::KeyframeRequest { .. } => {
                stream_control.request_keyframe();
            }
            WireMessage::KeyframeAck { .. } => {
                stream_control.acknowledge_keyframe();
            }
            WireMessage::RateHint {
                bitrate,
                frames_per_second,
            } => {
                stream_control.update_rate(bitrate, frames_per_second);
            }
            WireMessage::Close => {
                stream_control.shutdown.store(true, Ordering::Release);
                return Ok(());
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "windows")]
async fn send_desktop_frames(
    connection: &QuicConnection,
    stream_control: Arc<StreamControl>,
    hybrid_video: bool,
    desktop_optimizations: bool,
    h264_nal_datagrams: bool,
    profile: RateProfile,
    use_x264: bool,
) -> Result<()> {
    let source = WindowsDesktopCapturer::new(0, 1_000)?;
    send_frames_from(
        connection,
        stream_control,
        source,
        MediaFeatures {
            hybrid_video,
            desktop_optimizations,
            h264_nal_datagrams,
        },
        profile,
        move |frame, bitrate, fps, max_slice_len| {
            let size = frame.layout().size();
            if use_x264 {
                #[cfg(feature = "x264")]
                {
                    log(
                        LogLevel::Info,
                        "direct-computing::stream-host",
                        &format!(
                            "using x264 zerolatency software encoder bitrate={} fps={} transport_slice_budget={}",
                            bitrate, fps, max_slice_len
                        ),
                    );
                    return Ok(Box::new(X264Encoder::new(
                        size.width(),
                        size.height(),
                        bitrate,
                        fps,
                        max_slice_len,
                    )?) as Box<dyn dc_media::VideoEncoder>);
                }
                #[cfg(not(feature = "x264"))]
                {
                    return Err(DcError::Unsupported(
                        "--x264 requires rebuilding with --features x264 and a native libx264 installation"
                            .into(),
                    ));
                }
            }
            // Prefer the vendor-provided Media Foundation encoder at every
            // bitrate.  It is hardware accelerated when the host exposes a
            // compatible MFT and has much lower encode latency than the
            // software fallback.  OpenH264 remains the fallback if the MFT is
            // unavailable or rejects the requested profile.
            match WindowsMediaFoundationH264Encoder::new(size.width(), size.height(), bitrate, fps)
            {
                Ok(encoder) => {
                    if !encoder.capabilities().hardware_accelerated {
                        // Keep the system software MFT ahead of OpenH264. The
                        // latter is a compatibility fallback and is far too
                        // slow for a full-resolution interactive desktop on
                        // the tested host. MFT codec-control acceptance is
                        // logged by dc-platform for bitrate diagnostics.
                        log(
                            LogLevel::Info,
                            "direct-computing::stream-host",
                            "Media Foundation H.264 MFT is software-only; retaining MFT because OpenH264 is an emergency compatibility fallback",
                        );
                    }
                    Ok(Box::new(encoder))
                }
                Err(error) => {
                    log(
                        LogLevel::Warn,
                        "direct-computing::stream-host",
                        &format!("Media Foundation unavailable, using OpenH264 fallback: {error}"),
                    );
                    let minimum_qp = if bitrate <= 64_000 { 42 } else { 36 };
                    log(
                        LogLevel::Info,
                        "direct-computing::stream-host",
                        &format!(
                            "using OpenH264 low-quality fallback bitrate={} fps={} qp={}..51",
                            bitrate, fps, minimum_qp
                        ),
                    );
                    Ok(Box::new(OpenH264Encoder::new_network(
                        bitrate,
                        fps as f32,
                        minimum_qp,
                        max_slice_len,
                    )?))
                }
            }
        },
    )
    .await
}

#[cfg(not(target_os = "windows"))]
async fn send_desktop_frames(
    connection: &QuicConnection,
    stream_control: Arc<StreamControl>,
    hybrid_video: bool,
    desktop_optimizations: bool,
    h264_nal_datagrams: bool,
    profile: RateProfile,
    use_x264: bool,
) -> Result<()> {
    let size = FrameSize::new(640, 360)?;
    send_frames_from(
        connection,
        stream_control,
        SyntheticFrameSource::new(size, 30)?,
        MediaFeatures {
            hybrid_video,
            desktop_optimizations,
            h264_nal_datagrams,
        },
        profile,
        move |frame, bitrate, fps, max_slice_len| {
            if use_x264 {
                #[cfg(all(feature = "x264", target_os = "windows"))]
                {
                    let size = frame.layout().size();
                    return Ok(Box::new(X264Encoder::new(
                        size.width(),
                        size.height(),
                        bitrate,
                        fps,
                        max_slice_len,
                    )?) as Box<dyn dc_media::VideoEncoder>);
                }
                #[cfg(not(all(feature = "x264", target_os = "windows")))]
                {
                    return Err(DcError::Unsupported(
                        "--x264 requires rebuilding with --features x264 and a native libx264 installation"
                            .into(),
                    ));
                }
            }
            Ok(Box::new(OpenH264Encoder::new_network(
                bitrate,
                fps as f32,
                0,
                max_slice_len,
            )?))
        },
    )
    .await
}

async fn send_frames_from<S, F>(
    connection: &QuicConnection,
    stream_control: Arc<StreamControl>,
    mut source: S,
    features: MediaFeatures,
    profile: RateProfile,
    create_encoder: F,
) -> Result<()>
where
    S: dc_media::FrameSource + Send + 'static,
    F: Fn(&dc_media::VideoFrame, u32, u32, u32) -> Result<Box<dyn dc_media::VideoEncoder>>
        + Send
        + 'static,
{
    let MediaFeatures {
        hybrid_video,
        desktop_optimizations,
        h264_nal_datagrams,
    } = features;
    let max_datagram_size = connection.max_datagram_size().ok_or_else(|| {
        DcError::Unsupported("peer did not negotiate QUIC DATAGRAM support".into())
    })?;
    // Reserve space for the QUIC/media fragment envelope. Encoders that can
    // constrain NAL sizes use this value directly; other backends are still
    // packetized by the transport and observed by the oversize guard.
    let max_slice_len = u32::try_from(max_datagram_size.saturating_sub(128).max(256))
        .map_err(|_| DcError::InvalidInput("datagram size exceeds encoder limits".into()))?;
    // Keyframes are decoder recovery anchors. Carry them on a dedicated
    // reliable QUIC stream; ordinary inter frames stay on the lossy Datagram
    // lane so stale video never blocks input/control traffic.
    let mut keyframe_stream = if hybrid_video {
        Some(connection.open_stream().await?)
    } else {
        None
    };
    // Keep capture and encoding off the async runtime thread. The bounded
    // producer queue protects the runtime, while the datagram lane allows the
    // transport to discard stale media instead of blocking control traffic.
    let (packet_tx, mut packet_rx) = tokio::sync::mpsc::channel::<Arc<EncodedFrame>>(1);
    let producer_control = stream_control.clone();
    let producer_hybrid_video = hybrid_video;
    let producer = tokio::task::spawn_blocking(move || -> Result<()> {
        use dc_media::EncodeOutcome;
        let first_capture_started = Instant::now();
        let mut zero_frame_candidate = None;
        let first_frame = loop {
            match source.capture() {
                Ok(frame)
                    if is_effectively_zero_frame(&frame)
                        && first_capture_started.elapsed() < STARTUP_ZERO_FRAME_GRACE =>
                {
                    zero_frame_candidate = Some(frame);
                }
                Ok(frame) => break frame,
                Err(DcError::Timeout(_))
                    if first_capture_started.elapsed() >= STARTUP_ZERO_FRAME_GRACE
                        && zero_frame_candidate.is_some() =>
                {
                    break zero_frame_candidate
                        .take()
                        .expect("zero-frame candidate was checked");
                }
                Err(DcError::Timeout(_)) => continue,
                Err(error) => return Err(error),
            }
        };
        let first_frame = prepare_network_frame(first_frame, profile)?;
        let mut pending = (first_frame, first_capture_started.elapsed());
        let first_source_non_zero = pending.0.data().iter().filter(|byte| **byte > 4).count();
        let first_source_sample = pending
            .0
            .data()
            .iter()
            .take(64)
            .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
        log(
            LogLevel::Info,
            "direct-computing::stream-host",
            &format!(
                "first captured frame sequence={} sample_sum={} non_zero={} dimensions={}x{} color_depth={}bit",
                pending.0.sequence(),
                first_source_sample,
                first_source_non_zero,
                pending.0.layout().size().width(),
                pending.0.layout().size().height(),
                profile.color_depth_bits
            ),
        );
        let mut current_bitrate = producer_control.target_bitrate.load(Ordering::Acquire);
        let mut current_fps = producer_control.target_fps.load(Ordering::Acquire);
        let mut encoder = create_encoder(&pending.0, current_bitrate, current_fps, max_slice_len)?;
        let mut last_frame;
        let mut last_sequence;
        let capabilities = encoder.capabilities();
        log(
            LogLevel::Info,
            "direct-computing::stream-host",
            &format!(
                "encoder backend={} hardware={} zero_copy={} low_latency={} profile={} bitrate={} fps={}",
                capabilities.backend,
                capabilities.hardware_accelerated,
                capabilities.zero_copy_input,
                capabilities.low_latency,
                profile.name,
                current_bitrate,
                current_fps
            ),
        );
        log(
            LogLevel::Info,
            "direct-computing::stream-host",
            &format!(
                "video format=H.264 chroma={} color_depth={}bit",
                NETWORK_CHROMA_SUBSAMPLING, profile.color_depth_bits
            ),
        );
        let mut frame_interval = Duration::from_secs_f64(1.0 / f64::from(current_fps));
        let mut next_frame_at = std::time::Instant::now();
        let mut report_started = Instant::now();
        let mut captured = 0_u64;
        let mut encoded = 0_u64;
        let mut region_updates = 0_u64;
        let mut cursor_updates = 0_u64;
        let mut unchanged_skipped = 0_u64;
        let mut media_sequence = 0_u64;
        let mut cursor_sequence = 0_u64;
        let mut last_cursor: Option<dc_media::CursorUpdate> = None;
        let mut dirty_detector = DirtyRegionDetector::default();
        let mut last_full_frame_at = Instant::now()
            .checked_sub(FULL_FRAME_REFRESH_INTERVAL)
            .unwrap_or_else(Instant::now);
        let mut capture_total = Duration::ZERO;
        let mut encode_total = Duration::ZERO;
        let mut queue_dropped = 0_u64;
        loop {
            if producer_control.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
            // A reliable keyframe is the decoder's recovery barrier. Once it
            // has been emitted, do not process another desktop state until the
            // Viewer has decoded and acknowledged it. A pending request still
            // allows this iteration to produce the requested keyframe.
            while producer_hybrid_video
                && producer_control.recovery_pending.load(Ordering::Acquire)
                && !producer_control.keyframe_requested.load(Ordering::Acquire)
            {
                if producer_control.shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let (frame, capture_time) = pending;
            captured += 1;
            capture_total += capture_time;
            let recovery_frame = frame.clone();
            last_sequence = frame.sequence();
            let dirty_regions = dirty_detector.detect(&frame)?;
            if desktop_optimizations {
                if let Some(cursor) = frame.metadata().cursor.as_ref() {
                    if last_cursor.as_ref() != Some(cursor) {
                        let sequence = cursor_sequence;
                        let message = Arc::new(cursor_to_message(sequence, cursor));
                        cursor_sequence = cursor_sequence.wrapping_add(1);
                        let encoded_bytes = message.encode()?.len() as u64;
                        if packet_tx
                            .blocking_send(Arc::new(EncodedFrame {
                                message,
                                sequence,
                                encoded_bytes,
                                capture_time: Duration::ZERO,
                                encode_time: Duration::ZERO,
                                reliable_barrier: false,
                            }))
                            .is_err()
                        {
                            return Ok(());
                        }
                        last_cursor = Some(cursor.clone());
                        cursor_updates = cursor_updates.saturating_add(1);
                    }
                }
            }
            let target_bitrate = producer_control.target_bitrate.load(Ordering::Acquire);
            let target_fps = producer_control.target_fps.load(Ordering::Acquire);
            let settings_changed = target_bitrate != current_bitrate || target_fps != current_fps;
            let mut force_full_frame = settings_changed;
            if settings_changed {
                current_bitrate = target_bitrate;
                current_fps = target_fps;
                frame_interval = Duration::from_secs_f64(1.0 / f64::from(current_fps));
                encoder = create_encoder(&frame, current_bitrate, current_fps, max_slice_len)?;
                producer_control
                    .keyframe_requested
                    .store(false, Ordering::Release);
                log(
                    LogLevel::Info,
                    "direct-computing::stream-host",
                    &format!(
                        "adapted encoder bitrate={} fps={}",
                        current_bitrate, current_fps
                    ),
                );
            } else if producer_control
                .keyframe_requested
                .swap(false, Ordering::AcqRel)
            {
                force_full_frame = true;
                if let Err(error) = encoder.force_keyframe() {
                    log(
                        LogLevel::Debug,
                        "direct-computing::stream-host",
                        &format!("encoder could not force a keyframe: {error}"),
                    );
                    // Media Foundation encoders commonly do not expose a
                    // portable force-IDR control. Recreating the encoder
                    // resets its GOP and guarantees that the next packet is
                    // independently decodable.
                    encoder = create_encoder(&frame, current_bitrate, current_fps, max_slice_len)?;
                }
            }
            let frame_area = u64::from(frame.layout().size().width())
                * u64::from(frame.layout().size().height());
            let region_candidate = desktop_optimizations
                && !force_full_frame
                && !dirty_regions.is_empty()
                && last_full_frame_at.elapsed() < FULL_FRAME_REFRESH_INTERVAL
                && changed_area(&dirty_regions).saturating_mul(100)
                    <= frame_area.saturating_mul(REGION_UPDATE_MAX_AREA_PERCENT);
            let region_update = if region_candidate {
                let message = Arc::new(encode_region_update(
                    &frame,
                    &dirty_regions,
                    media_sequence,
                )?);
                let encoded_bytes = message.encode()?.len();
                (encoded_bytes <= REGION_UPDATE_MAX_BYTES).then_some((message, encoded_bytes))
            } else {
                None
            };
            if let Some((message, encoded_bytes)) = region_update {
                media_sequence = media_sequence.wrapping_add(1);
                let update = Arc::new(EncodedFrame {
                    sequence: video_sequence(&message).unwrap_or_default(),
                    encoded_bytes: encoded_bytes as u64,
                    capture_time,
                    encode_time: Duration::ZERO,
                    reliable_barrier: false,
                    message,
                });
                match packet_tx.try_send(update) {
                    Ok(()) => {
                        encoded = encoded.saturating_add(1);
                        region_updates = region_updates.saturating_add(1);
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        queue_dropped = queue_dropped.saturating_add(1);
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                }
            } else if dirty_regions.is_empty()
                && !force_full_frame
                && last_full_frame_at.elapsed() < FULL_FRAME_REFRESH_INTERVAL
            {
                // Pointer-only and unchanged frames do not enter the video
                // encoder. The periodic full refresh below bounds recovery.
                unchanged_skipped = unchanged_skipped.saturating_add(1);
            } else {
                let encode_started = Instant::now();
                if let EncodeOutcome::Packet(packet) = encoder.encode(frame)? {
                    let encode_time = encode_started.elapsed();
                    encode_total += encode_time;
                    encoded += 1;
                    // The source sequence advances for every capture attempt, but
                    // OpenH264/MFT may intentionally skip a frame.  Renumber only
                    // packets that really leave the encoder; otherwise the viewer
                    // mistakes encoder skips for network loss and requests a large
                    // reliable keyframe unnecessarily.
                    let packet = packet.with_sequence(media_sequence);
                    media_sequence = media_sequence.wrapping_add(1);
                    last_full_frame_at = Instant::now();
                    let keyframe = packet.is_keyframe();
                    let reliable_barrier = keyframe && (packet.sequence() == 0 || force_full_frame);
                    let encoded_frame = Arc::new(EncodedFrame {
                        sequence: packet.sequence(),
                        encoded_bytes: packet.data().len() as u64,
                        capture_time,
                        encode_time,
                        reliable_barrier,
                        message: Arc::new(packet_to_message(&packet)?),
                    });
                    if reliable_barrier && producer_hybrid_video {
                        // A recovery anchor must never be discarded because the
                        // queue currently contains an ordinary frame. Mark the
                        // barrier before enqueueing so no later capture can race
                        // ahead of the reliable keyframe.
                        producer_control
                            .recovery_pending
                            .store(true, Ordering::Release);
                        log(
                            LogLevel::Debug,
                            "direct-computing::stream-host",
                            &format!(
                                "keyframe queued sequence={}; pausing capture until Viewer acknowledgement",
                                packet.sequence()
                            ),
                        );
                        if packet_tx.blocking_send(encoded_frame).is_err() {
                            producer_control
                                .recovery_pending
                                .store(false, Ordering::Release);
                            return Ok(());
                        }
                    } else {
                        match packet_tx.try_send(encoded_frame) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                // Ordinary frames remain latest-frame oriented;
                                // dropping them is preferable to queueing stale
                                // desktop state behind a slow network.
                                queue_dropped = queue_dropped.saturating_add(1);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            last_frame = Some(recovery_frame);
            if report_started.elapsed() >= Duration::from_secs(1) {
                log(
                    LogLevel::Info,
                    "direct-computing::stream-host",
                    &format!(
                        "capture={} encoded={} regions={} cursors={} unchanged_skipped={} queue_dropped={} capture_avg={:.2}ms encode_avg={:.2}ms",
                        captured,
                        encoded,
                        region_updates,
                        cursor_updates,
                        unchanged_skipped,
                        queue_dropped,
                        average_millis(capture_total, captured),
                        average_millis(encode_total, encoded)
                    ),
                );
                captured = 0;
                encoded = 0;
                region_updates = 0;
                cursor_updates = 0;
                unchanged_skipped = 0;
                queue_dropped = 0;
                capture_total = Duration::ZERO;
                encode_total = Duration::ZERO;
                report_started = Instant::now();
            }
            next_frame_at += frame_interval;
            let now = std::time::Instant::now();
            if now < next_frame_at {
                std::thread::sleep(next_frame_at - now);
            } else {
                next_frame_at = now;
            }
            while producer_hybrid_video
                && producer_control.recovery_pending.load(Ordering::Acquire)
                && !producer_control.keyframe_requested.load(Ordering::Acquire)
            {
                if producer_control.shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let capture_started = Instant::now();
            pending = loop {
                match source.capture() {
                    Err(DcError::Timeout(_))
                        if producer_control.shutdown.load(Ordering::Acquire) =>
                    {
                        return Ok(())
                    }
                    Err(DcError::Timeout(_))
                        if producer_control.keyframe_requested.load(Ordering::Acquire) =>
                    {
                        let previous = last_frame.as_ref().ok_or_else(|| {
                            DcError::Platform(
                                "keyframe requested before a recoverable frame existed".into(),
                            )
                        })?;
                        let sequence = last_sequence.wrapping_add(1);
                        let recovered = dc_media::VideoFrame::new(
                            sequence,
                            // Desktop Duplication does not deliver a new
                            // texture while the desktop is static.  Give a
                            // retransmitted recovery frame a fresh,
                            // monotonically increasing timestamp; reusing the
                            // previous timestamp can make H.264 encoders
                            // classify it as a duplicate and return Skip,
                            // leaving the viewer with only its initial frame.
                            previous.timestamp().saturating_add(frame_interval),
                            previous.layout(),
                            previous.data().to_vec(),
                        )?;
                        log(
                            LogLevel::Debug,
                            "direct-computing::stream-host",
                            &format!(
                                "desktop unchanged; synthesized recovery frame sequence={} timestamp_ms={}",
                                recovered.sequence(),
                                recovered.timestamp().as_millis()
                            ),
                        );
                        break (recovered, Duration::ZERO);
                    }
                    Err(DcError::Timeout(_))
                        if desktop_optimizations
                            && last_full_frame_at.elapsed() >= FULL_FRAME_REFRESH_INTERVAL
                            && last_frame.is_some() =>
                    {
                        let previous = last_frame.as_ref().expect("last frame was checked");
                        let sequence = last_sequence.wrapping_add(1);
                        let refresh = dc_media::VideoFrame::new(
                            sequence,
                            previous
                                .timestamp()
                                .saturating_add(FULL_FRAME_REFRESH_INTERVAL),
                            previous.layout(),
                            previous.data().to_vec(),
                        )?;
                        break (refresh, Duration::ZERO);
                    }
                    Err(DcError::Timeout(_)) => continue,
                    result => {
                        let frame = prepare_network_frame(result?, profile)?;
                        break (frame, capture_started.elapsed());
                    }
                }
            };
        }
    });

    let mut sent = 0_u64;
    let mut datagrams_sent = 0_u64;
    let mut oversized_dropped = 0_u64;
    let mut recovery_wait_dropped = 0_u64;
    let mut first_frame_logged = false;
    let mut send_total = Duration::ZERO;
    let mut send_report_started = Instant::now();
    while !stream_control.shutdown.load(Ordering::Acquire) {
        let Some(frame) = packet_rx.recv().await else {
            break;
        };
        let send_started = Instant::now();
        if matches!(frame.message.as_ref(), WireMessage::Cursor { .. }) {
            keyframe_stream
                .as_mut()
                .ok_or_else(|| DcError::Codec("cursor update requires reliable media".into()))?
                .send(frame.message.as_ref())
                .await?;
            continue;
        }
        let keyframe = matches!(
            frame.message.as_ref(),
            WireMessage::Video { keyframe: true, .. }
        );
        if !first_frame_logged {
            log(
                LogLevel::Info,
                "direct-computing::stream-host",
                &format!(
                    "first video frame sequence={} bytes={} keyframe={} transport={}",
                    frame.sequence,
                    frame.encoded_bytes,
                    keyframe,
                    if keyframe && hybrid_video {
                        "reliable-stream"
                    } else {
                        "quic-datagram"
                    }
                ),
            );
            first_frame_logged = true;
        }

        if keyframe && hybrid_video {
            keyframe_stream
                .as_mut()
                .expect("hybrid media stream was opened")
                .send(frame.message.as_ref())
                .await?;
            if frame.reliable_barrier {
                stream_control
                    .recovery_pending
                    .store(true, Ordering::Release);
                // A recovery keyframe can take seconds to cross a narrow
                // link. Discard frames captured while the barrier was in
                // flight. Natural periodic IDRs are reliable too, but do not
                // pause capture or flush newer media.
                let mut stale_frames = 0_u64;
                while packet_rx.try_recv().is_ok() {
                    stale_frames = stale_frames.saturating_add(1);
                }
                if stale_frames > 0 {
                    log(
                        LogLevel::Debug,
                        "direct-computing::stream-host",
                        &format!(
                            "discarded {} stale encoded frames after recovery keyframe sequence={}",
                            stale_frames, frame.sequence
                        ),
                    );
                }
            }
        } else {
            if !keyframe && hybrid_video && stream_control.recovery_pending.load(Ordering::Acquire)
            {
                recovery_wait_dropped = recovery_wait_dropped.saturating_add(1);
                continue;
            }
            let fragments = if h264_nal_datagrams
                && matches!(frame.message.as_ref(), WireMessage::Video { codec: 1, .. })
            {
                packetize_h264_nal_message(frame.message.as_ref(), max_datagram_size)?
            } else {
                packetize_video_message(frame.message.as_ref(), max_datagram_size)?
            };
            if !h264_nal_datagrams
                && !keyframe
                && fragments.len() > MAX_DATAGRAM_FRAGMENTS_PER_FRAME
            {
                oversized_dropped = oversized_dropped.saturating_add(1);
                if let Some((bitrate, fps)) = stream_control.request_lower_rate_keyframe() {
                    log(
                        LogLevel::Warn,
                        "direct-computing::stream-host",
                        &format!(
                            "dropped oversized inter frame sequence={} bytes={} fragments={}; requested one reliable keyframe and reduced rate to bitrate={} fps={}",
                            frame.sequence,
                            frame.encoded_bytes,
                            fragments.len(),
                            bitrate,
                            fps
                        ),
                    );
                }
                continue;
            }
            for fragment in fragments {
                // `send_datagram` evicts older queued Datagram payloads when
                // Quinn's send buffer fills. A large frame would therefore
                // discard its own leading fragments and become impossible to
                // reassemble. Wait for buffer space so each submitted frame is
                // internally complete; the one-frame producer queue still
                // drops newer stale work under sustained backpressure.
                connection.send_datagram_wait(fragment.into()).await?;
                datagrams_sent = datagrams_sent.saturating_add(1);
            }
            if keyframe && !hybrid_video {
                // Legacy peers cannot acknowledge a keyframe. Allow a later
                // recovery request after the Datagram send has been queued.
                stream_control.acknowledge_keyframe();
            }
        }
        let send_time = send_started.elapsed();
        sent += 1;
        send_total += send_time;
        if send_time >= Duration::from_millis(20) {
            log(
                LogLevel::Debug,
                "direct-computing::stream-host",
                &format!(
                    "send backpressure: sequence={} bytes={} wait={:.2}ms capture={:.2}ms encode={:.2}ms",
                    frame.sequence,
                    frame.encoded_bytes,
                    send_time.as_secs_f64() * 1_000.0,
                    frame.capture_time.as_secs_f64() * 1_000.0,
                    frame.encode_time.as_secs_f64() * 1_000.0
                ),
            );
        }
        if send_report_started.elapsed() >= Duration::from_secs(1) {
            log(
                LogLevel::Info,
                "direct-computing::stream-host",
                &format!(
                    "sent={} send_avg={:.2}ms",
                    sent,
                    average_millis(send_total, sent)
                ),
            );
            log(
                LogLevel::Info,
                "direct-computing::stream-host",
                &format!(
                    "video datagrams sent={} oversized_dropped={} recovery_wait_dropped={} bitrate={} fps={} rtt_ms={:.1}",
                    datagrams_sent,
                    oversized_dropped,
                    recovery_wait_dropped,
                    stream_control.target_bitrate.load(Ordering::Acquire),
                    stream_control.target_fps.load(Ordering::Acquire),
                    connection.rtt().as_secs_f64() * 1_000.0
                ),
            );
            sent = 0;
            datagrams_sent = 0;
            oversized_dropped = 0;
            recovery_wait_dropped = 0;
            send_total = Duration::ZERO;
            send_report_started = Instant::now();
        }
    }
    producer
        .await
        .map_err(|error| DcError::Platform(format!("capture task failed: {error}")))??;
    Ok(())
}

fn video_sequence(message: &WireMessage) -> Option<u64> {
    match message {
        WireMessage::Video { sequence, .. } | WireMessage::DesktopUpdate { sequence, .. } => {
            Some(*sequence)
        }
        _ => None,
    }
}

fn cursor_to_message(sequence: u64, cursor: &dc_media::CursorUpdate) -> WireMessage {
    WireMessage::Cursor {
        sequence,
        visible: cursor.visible,
        x: cursor.x,
        y: cursor.y,
        shape: cursor.shape.as_ref().map(|shape| dc_protocol::CursorShape {
            kind: match shape.kind {
                dc_media::CursorShapeKind::Monochrome => 1,
                dc_media::CursorShapeKind::Color => 2,
                dc_media::CursorShapeKind::MaskedColor => 4,
            },
            width: shape.width,
            height: shape.height,
            hotspot_x: shape.hotspot_x,
            hotspot_y: shape.hotspot_y,
            pitch: shape.pitch,
            data: shape.data.clone(),
        }),
    }
}

fn is_effectively_zero_frame(frame: &dc_media::VideoFrame) -> bool {
    frame.data().iter().all(|byte| *byte <= 4)
}

fn prepare_network_frame(frame: VideoFrame, profile: RateProfile) -> Result<VideoFrame> {
    if frame.layout().pixel_format() != PixelFormat::Bgra32 {
        return Err(DcError::Unsupported(
            "network color-depth reduction currently requires BGRA32 input".into(),
        ));
    }
    let layout = frame.layout();
    let metadata = frame.metadata().clone();
    let mut data = frame.data().to_vec();
    for row in data.chunks_exact_mut(layout.stride()) {
        for pixel in row[..layout.size().width() as usize * 4].chunks_exact_mut(4) {
            // Keep per-pixel geometry and edge detail intact. Only quantize
            // color channels; spatial filtering is intentionally disabled
            // because it makes text and small controls hard to read.
            pixel[0] &= profile.color_masks[0];
            pixel[1] &= profile.color_masks[1];
            pixel[2] &= profile.color_masks[2];
        }
    }
    VideoFrame::new(frame.sequence(), frame.timestamp(), layout, data)?.with_metadata(metadata)
}

fn reduce_stream_rate(
    bitrate: u32,
    frames_per_second: u16,
    min_bitrate: u32,
    min_fps: u32,
) -> (u32, u16) {
    (
        (bitrate.saturating_mul(2) / 3).max(min_bitrate),
        (u32::from(frames_per_second) * 2 / 3).max(min_fps) as u16,
    )
}

fn increase_stream_rate(
    bitrate: u32,
    frames_per_second: u16,
    max_bitrate: u32,
    max_fps: u32,
) -> (u32, u16) {
    (
        bitrate
            .saturating_mul(4)
            .saturating_div(3)
            .saturating_add(1)
            .min(max_bitrate),
        u32::from(frames_per_second).saturating_add(1).min(max_fps) as u16,
    )
}

#[cfg(target_os = "macos")]
struct MacViewerDecoder {
    hardware: Option<VideoToolboxH264Decoder>,
    fallback: OpenH264Decoder,
}

#[cfg(target_os = "macos")]
impl dc_media::VideoDecoder for MacViewerDecoder {
    fn codec(&self) -> dc_media::VideoCodec {
        dc_media::VideoCodec::H264
    }

    fn capabilities(&self) -> dc_media::DecoderCapabilities {
        match &self.hardware {
            Some(decoder) => dc_media::VideoDecoder::capabilities(decoder),
            None => dc_media::VideoDecoder::capabilities(&self.fallback),
        }
    }

    fn decode(&mut self, packet: dc_media::EncodedVideoPacket) -> Result<dc_media::VideoFrame> {
        if let Some(decoder) = &mut self.hardware {
            match dc_media::VideoDecoder::decode(decoder, packet.clone()) {
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    // Inter frames can legitimately become undecodable after
                    // loss on the QUIC DATAGRAM lane. Keep VideoToolbox alive
                    // so the viewer can recover on the next keyframe instead
                    // of permanently falling back to software after one gap.
                    if !packet.is_keyframe() {
                        return Err(error);
                    }
                    log(
                        LogLevel::Warn,
                        "direct-computing::stream-viewer",
                        &format!("VideoToolbox keyframe decode failed, trying OpenH264: {error}"),
                    );
                    match dc_media::VideoDecoder::decode(&mut self.fallback, packet) {
                        Ok(frame) => {
                            self.hardware = None;
                            return Ok(frame);
                        }
                        Err(fallback_error) => {
                            return Err(DcError::Codec(format!(
                                "VideoToolbox decode failed: {error}; OpenH264 fallback failed: {fallback_error}"
                            )));
                        }
                    }
                }
            }
        }
        dc_media::VideoDecoder::decode(&mut self.fallback, packet)
    }
}

#[cfg(target_os = "windows")]
struct WindowsViewerDecoder {
    hardware: Option<WindowsMediaFoundationH264Decoder>,
    hardware_attempted: bool,
    fallback: OpenH264Decoder,
}

#[cfg(target_os = "windows")]
impl dc_media::VideoDecoder for WindowsViewerDecoder {
    fn codec(&self) -> dc_media::VideoCodec {
        dc_media::VideoCodec::H264
    }

    fn capabilities(&self) -> dc_media::DecoderCapabilities {
        match &self.hardware {
            Some(decoder) => dc_media::VideoDecoder::capabilities(decoder),
            None => dc_media::VideoDecoder::capabilities(&self.fallback),
        }
    }

    fn decode(&mut self, packet: dc_media::EncodedVideoPacket) -> Result<dc_media::VideoFrame> {
        if self.hardware.is_none() && !self.hardware_attempted {
            self.hardware_attempted = true;
            let size = packet.source_layout().size();
            match WindowsMediaFoundationH264Decoder::new(size.width(), size.height()) {
                Ok(decoder) => {
                    let capabilities = dc_media::VideoDecoder::capabilities(&decoder);
                    log(
                        LogLevel::Info,
                        "direct-computing::stream-viewer",
                        &format!(
                            "decoder backend={} hardware={} zero_copy={} low_latency={}",
                            capabilities.backend,
                            capabilities.hardware_accelerated,
                            capabilities.zero_copy_output,
                            capabilities.low_latency
                        ),
                    );
                    self.hardware = Some(decoder);
                }
                Err(error) => log(
                    LogLevel::Warn,
                    "direct-computing::stream-viewer",
                    &format!("Media Foundation decoder unavailable, using OpenH264: {error}"),
                ),
            }
        }
        if let Some(decoder) = &mut self.hardware {
            match dc_media::VideoDecoder::decode(decoder, packet.clone()) {
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    log(
                        LogLevel::Warn,
                        "direct-computing::stream-viewer",
                        &format!("Media Foundation decode failed, switching to OpenH264: {error}"),
                    );
                    self.hardware = None;
                }
            }
        }
        dc_media::VideoDecoder::decode(&mut self.fallback, packet)
    }
}

#[cfg(target_os = "windows")]
fn create_viewer_decoder() -> Result<Box<dyn dc_media::VideoDecoder>> {
    // The stream dimensions are learned from the first packet, so the
    // Windows MFT is initialized lazily in run_network_viewer.
    Ok(Box::new(WindowsViewerDecoder {
        hardware: None,
        hardware_attempted: false,
        fallback: OpenH264Decoder::new()?,
    }))
}

#[cfg(not(target_os = "windows"))]
fn create_viewer_decoder() -> Result<Box<dyn dc_media::VideoDecoder>> {
    #[cfg(target_os = "macos")]
    {
        match VideoToolboxH264Decoder::new() {
            Ok(decoder) => {
                let decoder = MacViewerDecoder {
                    hardware: Some(decoder),
                    fallback: OpenH264Decoder::new()?,
                };
                let capabilities = dc_media::VideoDecoder::capabilities(&decoder);
                log(
                    LogLevel::Info,
                    "direct-computing::stream-viewer",
                    &format!(
                        "decoder backend={} hardware={} zero_copy={} low_latency={}",
                        capabilities.backend,
                        capabilities.hardware_accelerated,
                        capabilities.zero_copy_output,
                        capabilities.low_latency
                    ),
                );
                return Ok(Box::new(decoder));
            }
            Err(error) => log(
                LogLevel::Warn,
                "direct-computing::stream-viewer",
                &format!("VideoToolbox unavailable, using OpenH264: {error}"),
            ),
        }
    }

    let decoder = OpenH264Decoder::new()?;
    let capabilities = dc_media::VideoDecoder::capabilities(&decoder);
    log(
        LogLevel::Info,
        "direct-computing::stream-viewer",
        &format!(
            "decoder backend={} hardware={} zero_copy={} low_latency={}",
            capabilities.backend,
            capabilities.hardware_accelerated,
            capabilities.zero_copy_output,
            capabilities.low_latency
        ),
    );
    Ok(Box::new(decoder))
}

fn run_network_viewer(
    address: &str,
    password: &str,
    fingerprint: Option<&str>,
    profile: RateProfile,
    show_dirty_regions: bool,
) -> Result<()> {
    let address = address.to_owned();
    let password = password.to_owned();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| DcError::Platform(error.to_string()))?
        .block_on(async move {
            let client_address = "0.0.0.0:0".parse().unwrap();
            let client = match fingerprint {
                Some(value) => QuicClient::bind_pinned(client_address, parse_fingerprint(value)?)?,
                None => QuicClient::bind_tofu(client_address)?,
            };
            let connection = client.connect(dc_transport::resolve_address(&address).await?).await?;
            if fingerprint.is_none() {
                let observed = client.server_certificate_fingerprint().ok_or_else(|| {
                    DcError::Platform("server certificate fingerprint was not observed".into())
                })?;
                log(
                    LogLevel::Warn,
                    "direct-computing::viewer",
                    &format!(
                        "first connection certificate sha256={}; reconnect with this fingerprint to pin it",
                        format_fingerprint(observed)
                    ),
                );
            }
            let mut control = connection.open_stream().await?;
            let session = authenticate_client(
                &mut control,
                &password,
                Capabilities {
                    desktop: true,
                    control_input: true,
                    hybrid_video: true,
                    desktop_optimizations: true,
                    h264_nal_datagrams: true,
                    ..Capabilities::default()
                },
            )
            .await?;
            if !session.permissions.view_desktop {
                return Err(DcError::Unsupported(
                    "desktop viewing is not permitted".into(),
                ));
            }
            let host_hybrid_video = session.peer_capabilities.hybrid_video;
            let host_desktop_optimizations =
                host_hybrid_video && session.peer_capabilities.desktop_optimizations;
            let host_h264_nal_datagrams =
                host_hybrid_video && session.peer_capabilities.h264_nal_datagrams;
            let _ = control.receive().await?;
            let max_datagram_size = connection.max_datagram_size();
            log(
                LogLevel::Info,
                "direct-computing::stream-viewer",
                &format!(
                    "media protocol={} host_hybrid_video={} video transport={} max_datagram_size={:?} profile={} bitrate={} fps={}",
                    MEDIA_PROTOCOL_REVISION,
                    host_hybrid_video,
                    if host_h264_nal_datagrams {
                        "hybrid-keyframe-stream+h264-nal-datagram"
                    } else if host_hybrid_video {
                        "hybrid-keyframe-stream+quic-datagram"
                    } else {
                        "legacy-quic-datagram"
                    },
                    max_datagram_size,
                    profile.name,
                    profile.initial_bitrate,
                    profile.initial_fps
                ),
            );
            if max_datagram_size.is_none() {
                return Err(DcError::Unsupported(
                    "peer does not support QUIC DATAGRAM video; update the Host and reconnect"
                        .into(),
                ));
            }
            // Capability negotiation already distinguishes old Hosts, so a
            // new Host's media stream is accepted without an arbitrary
            // latency timeout.
            let mut keyframe_stream = if host_hybrid_video {
                Some(connection.accept_stream().await?)
            } else {
                None
            };
            if keyframe_stream.is_none() {
                log(
                    LogLevel::Warn,
                    "direct-computing::stream-viewer",
                    "Host does not advertise hybrid media; using legacy Datagram keyframes",
                );
            }
            let mut decoder = create_viewer_decoder()?;
            let mut sink = PreviewWindowSink::new("Direct Computing - Remote Desktop");
            sink.set_dirty_region_debug(show_dirty_regions);
            sink.show_placeholder(640, 360)?;
            log(
                LogLevel::Info,
                "direct-computing::stream-viewer",
                &format!("present backend={}", PreviewWindowSink::render_backend()),
            );
            let mut desktop_size = None;
            // Keyframes arrive on a dedicated reliable stream. Inter frames
            // use unreliable, unordered QUIC DATAGRAM; incomplete or stale
            // frames are discarded without blocking control/input traffic.
            let (keyframe_tx, mut keyframe_rx) = tokio::sync::mpsc::channel::<
                std::result::Result<ReceivedFrame, String>,
            >(2);
            if let Some(mut keyframe_stream) = keyframe_stream.take() {
                tokio::spawn(async move {
                    loop {
                        match keyframe_stream.receive().await {
                        Ok(message)
                            if matches!(
                                &message,
                                WireMessage::Video { keyframe: true, .. }
                                    | WireMessage::Cursor { .. }
                            ) =>
                        {
                            let received_at = Instant::now();
                            if keyframe_tx
                                .send(Ok(ReceivedFrame {
                                    message: Arc::new(message),
                                    received_at,
                                }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(_) => {
                            let _ = keyframe_tx
                                .send(Err("reliable media stream received a non-keyframe".into()))
                                .await;
                            break;
                        }
                        Err(error) => {
                            let _ = keyframe_tx.send(Err(error.to_string())).await;
                            break;
                        }
                        }
                    }
                });
            }
            let (video_tx, mut video_rx) = tokio::sync::mpsc::channel::<
                std::result::Result<ReceivedFrame, String>,
            >(4);
            let datagrams_received = Arc::new(AtomicU64::new(0));
            let receiver_datagrams = datagrams_received.clone();
            let datagram_connection = connection.clone();
            tokio::spawn(async move {
                let mut frame_reassembler = VideoDatagramReassembler::new();
                let mut nal_reassembler = H264NalDatagramReassembler::new();
                let mut completed_frames = 0_u64;
                // The local path also supports negotiated per-NAL datagrams;
                // the reassemblers below keep that capability while
                // preserving the reliable keyframe queue.
                loop {
                    match datagram_connection.receive_datagram().await {
                        Ok(datagram) => {
                            let received_at = Instant::now();
                            let received_count = receiver_datagrams
                                .fetch_add(1, Ordering::Relaxed)
                                .saturating_add(1);
                            if received_count <= 3 {
                                log(
                                    LogLevel::Info,
                                    "direct-computing::stream-viewer",
                                    &format!(
                                        "received video datagram index={} bytes={}",
                                        received_count,
                                        datagram.len()
                                    ),
                                );
                            }
                            let reassembled = match classify_video_datagram(&datagram) {
                                Some(VideoDatagramKind::H264Nal) if host_h264_nal_datagrams => {
                                    nal_reassembler.push(&datagram)
                                }
                                Some(VideoDatagramKind::Legacy) => {
                                    frame_reassembler.push(&datagram)
                                }
                                Some(VideoDatagramKind::H264Nal) => {
                                    log(
                                        LogLevel::Warn,
                                        "direct-computing::stream-viewer",
                                        "dropping NAL datagram from a peer without NAL capability",
                                    );
                                    continue;
                                }
                                None => {
                                    log(
                                        LogLevel::Warn,
                                        "direct-computing::stream-viewer",
                                        &format!(
                                            "dropping unknown video datagram envelope bytes={}",
                                            datagram.len()
                                        ),
                                    );
                                    continue;
                                }
                            };
                            match reassembled {
                            Ok(Some(message)) => {
                                completed_frames = completed_frames.saturating_add(1);
                                if completed_frames == 1 {
                                    log(
                                        LogLevel::Info,
                                        "direct-computing::stream-viewer",
                                        &format!(
                                            "received first complete inter frame after {} datagrams",
                                            received_count
                                        ),
                                    );
                                }
                                let keyframe = matches!(
                                    &message,
                                    WireMessage::Video { keyframe: true, .. }
                                );
                                let update = Ok(ReceivedFrame {
                                    message: Arc::new(message),
                                    received_at,
                                });
                                if keyframe {
                                    if video_tx.send(update).await.is_err() {
                                        break;
                                    }
                                } else {
                                    match video_tx.try_send(update) {
                                        Ok(()) => {}
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                            break;
                                        }
                                    }
                                }
                            }
                                Ok(None) => continue,
                                Err(error) => {
                                    log(
                                        LogLevel::Warn,
                                        "direct-computing::stream-viewer",
                                        &format!("dropping rejected video datagram: {error}"),
                                    );
                                    continue;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = video_tx.send(Err(error.to_string())).await;
                            break;
                        }
                    }
                }
            });
            let mut last_sequence: Option<u64> = None;
            let mut last_keyframe_request: Option<Instant> = None;
            let mut last_frame_progress = Instant::now();
            let mut datagrams_at_last_frame = 0_u64;
            let mut requested_bitrate = profile.initial_bitrate;
            let mut requested_fps = profile.initial_fps as u16;
            let mut stable_inter_frames = 0_u32;
            let mut stable_since = Instant::now();
            // Explicitly publish the selected profile's startup target so a
            // Host cannot accidentally begin at a cached rate from a prior
            // session.
            control
                .send(&WireMessage::RateHint {
                    bitrate: requested_bitrate,
                    frames_per_second: requested_fps,
                })
                .await?;
            let mut waiting_for_keyframe = true;
            let mut packet_logged = false;
            let mut decoded_logged = false;
            let mut content_frame_logged = false;
            let mut presented = 0_u64;
            let mut dropped = 0_u64;
            let mut decode_total = Duration::ZERO;
            let mut present_total = Duration::ZERO;
            let mut receive_to_present_total = Duration::ZERO;
            let mut report_started = Instant::now();
            while sink.is_open() {
                let update = match keyframe_rx.try_recv() {
                    Ok(update) => Some(update),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        match tokio::time::timeout(Duration::from_millis(16), video_rx.recv()).await
                        {
                            Ok(Some(update)) => Some(update),
                            Ok(None) => {
                                return Err(DcError::Platform(
                                    "video datagram receiver closed".into(),
                                ));
                            }
                            Err(_) => None,
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => None,
                };
                if let Some(update) = update {
                    let received = match update {
                        Ok(received) => received,
                        Err(error) => {
                            return Err(DcError::Platform(format!(
                                "video stream closed: {error}"
                            )));
                        }
                    };
                    if let WireMessage::Cursor {
                        visible,
                        x,
                        y,
                        shape,
                        ..
                    } = received.message.as_ref()
                    {
                        if !host_desktop_optimizations {
                            return Err(DcError::Codec(
                                "Host sent an unnegotiated cursor update".into(),
                            ));
                        }
                        sink.update_remote_cursor(*visible, *x, *y, shape.clone())?;
                        continue;
                    }
                    last_frame_progress = Instant::now();
                    datagrams_at_last_frame = datagrams_received.load(Ordering::Relaxed);
                    let sequence = video_sequence(received.message.as_ref());
                    let mut sequence_gap = false;
                    if let (Some(previous), Some(current)) = (last_sequence, sequence) {
                        if current <= previous {
                            continue;
                        }
                        let gap = current.saturating_sub(previous.saturating_add(1));
                        sequence_gap = gap > 0;
                    }
                    let message_is_keyframe = matches!(
                        received.message.as_ref(),
                        WireMessage::Video { keyframe: true, .. }
                    );
                    if let (Some(previous), Some(current)) = (last_sequence, sequence) {
                        let gap = current.saturating_sub(previous.saturating_add(1));
                        dropped = dropped.saturating_add(gap);
                        if gap > 0 && !message_is_keyframe {
                            // A sequence gap can be an intentionally replaced
                            // latest-frame update, a lost region update, or a
                            // missing H.264 reference. Let the decoder conceal
                            // the loss and accept the complete current frame.
                            // Request recovery only if decode actually fails or
                            // no complete media frame arrives for the watchdog
                            // interval; immediately discarding this packet made
                            // every isolated loss turn into a keyframe storm.
                            stable_inter_frames = 0;
                            stable_since = Instant::now();
                        }
                    }
                    if let WireMessage::DesktopUpdate {
                        sequence,
                        desktop_width,
                        desktop_height,
                        regions,
                        ..
                    } = received.message.as_ref()
                    {
                        if !host_desktop_optimizations {
                            return Err(DcError::Codec(
                                "Host sent an unnegotiated desktop region update".into(),
                            ));
                        }
                        let payload = decode_region_payload(received.message.as_ref())?;
                        let present_started = Instant::now();
                        sink.apply_bgra_regions(
                            *desktop_width,
                            *desktop_height,
                            regions,
                            &payload,
                        )?;
                        present_total += present_started.elapsed();
                        receive_to_present_total += received.received_at.elapsed();
                        presented = presented.saturating_add(1);
                        last_sequence = Some(*sequence);
                        stable_inter_frames = stable_inter_frames.saturating_add(1);
                        continue;
                    }
                    let decode_started = Instant::now();
                    let packet = dc_desktop::message_to_packet_ref(received.message.as_ref())?;
                    let packet_is_keyframe = packet.is_keyframe();
                    desktop_size = Some((
                        packet.source_layout().size().width(),
                        packet.source_layout().size().height(),
                    ));
                    if !packet_logged {
                        let sample = packet
                            .data()
                            .iter()
                            .take(32)
                            .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
                        log(
                            LogLevel::Info,
                            "direct-computing::stream-viewer",
                            &format!(
                                "first encoded frame sequence={} keyframe={} bytes={} sample_sum={} dimensions={}x{}",
                                packet.sequence(),
                                packet.is_keyframe(),
                                packet.data().len(),
                                sample,
                                packet.source_layout().size().width(),
                                packet.source_layout().size().height()
                            ),
                        );
                        packet_logged = true;
                    }
                    if sequence_gap {
                        waiting_for_keyframe = true;
                    }
                    if waiting_for_keyframe && !packet_is_keyframe {
                        if last_keyframe_request
                            .is_none_or(|time| time.elapsed() >= Duration::from_secs(1))
                        {
                            control
                                .send(&WireMessage::KeyframeRequest {
                                    last_sequence: sequence.unwrap_or_default(),
                                })
                                .await?;
                            last_keyframe_request = Some(Instant::now());
                            log(
                                LogLevel::Debug,
                                "direct-computing::stream-viewer",
                                "waiting for a recovery keyframe; dropped dependent frame",
                            );
                        }
                        continue;
                    }
                    if waiting_for_keyframe {
                        // Reset VideoToolbox/OpenH264 before consuming the
                        // recovery keyframe so no references from the damaged
                        // GOP survive into the new decode chain.
                        decoder = create_viewer_decoder()?;
                    }
                    let frame = match decoder.decode(packet) {
                        Ok(frame) => {
                            waiting_for_keyframe = false;
                            frame
                        }
                        Err(error) => {
                            waiting_for_keyframe = true;
                            if last_keyframe_request
                                .is_none_or(|time| time.elapsed() >= Duration::from_secs(1))
                            {
                                control
                                    .send(&WireMessage::KeyframeRequest {
                                        last_sequence: sequence.unwrap_or_default(),
                                    })
                                    .await?;
                                last_keyframe_request = Some(Instant::now());
                            }
                            log(
                                LogLevel::Warn,
                                "direct-computing::stream-viewer",
                                &format!("dropping undecodable video frame: {error}"),
                            );
                            continue;
                        }
                    };
                    if !decoded_logged || !content_frame_logged {
                        let sample = frame
                            .data()
                            .iter()
                            .take(64)
                            .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
                        let non_zero = frame.data().iter().filter(|byte| **byte > 4).count();
                        if !decoded_logged {
                            log(
                                LogLevel::Info,
                                "direct-computing::stream-viewer",
                                &format!(
                                    "first decoded frame sequence={} bytes={} sample_sum={} non_zero={} dimensions={}x{}",
                                    frame.sequence(),
                                    frame.data().len(),
                                    sample,
                                    non_zero,
                                    frame.layout().size().width(),
                                    frame.layout().size().height()
                                ),
                            );
                            decoded_logged = true;
                        }
                        if non_zero > 0 && !content_frame_logged {
                            log(
                                LogLevel::Info,
                                "direct-computing::stream-viewer",
                                &format!(
                                    "first non-black decoded frame sequence={} non_zero={}",
                                    frame.sequence(), non_zero
                                ),
                            );
                            content_frame_logged = true;
                        }
                    }
                    let presented_sequence = frame.sequence();
                    decode_total += decode_started.elapsed();
                    let present_started = Instant::now();
                    dc_media::FrameSink::present(&mut sink, frame)?;
                    present_total += present_started.elapsed();
                    receive_to_present_total += received.received_at.elapsed();
                    presented += 1;
                    last_sequence = Some(presented_sequence);
                    if packet_is_keyframe && host_hybrid_video {
                        control
                            .send(&WireMessage::KeyframeAck {
                                sequence: presented_sequence,
                            })
                            .await?;
                    }
                    if packet_is_keyframe {
                        stable_inter_frames = 0;
                        stable_since = Instant::now();
                    } else {
                        stable_inter_frames = stable_inter_frames.saturating_add(1);
                    }
                }
                let current_datagrams = datagrams_received.load(Ordering::Relaxed);
                let incomplete_datagrams = current_datagrams > datagrams_at_last_frame;
                if last_frame_progress.elapsed() >= Duration::from_secs(2)
                    && incomplete_datagrams
                    && last_keyframe_request
                        .is_none_or(|time| time.elapsed() >= Duration::from_secs(1))
                {
                    if incomplete_datagrams {
                        (requested_bitrate, requested_fps) = reduce_stream_rate(
                            requested_bitrate,
                            requested_fps,
                            profile.min_bitrate,
                            profile.min_fps,
                        );
                        control
                            .send(&WireMessage::RateHint {
                                bitrate: requested_bitrate,
                                frames_per_second: requested_fps,
                            })
                            .await?;
                    }
                    control
                        .send(&WireMessage::KeyframeRequest {
                            last_sequence: last_sequence.unwrap_or_default(),
                        })
                        .await?;
                    last_keyframe_request = Some(Instant::now());
                    last_frame_progress = Instant::now();
                    datagrams_at_last_frame = current_datagrams;
                    stable_inter_frames = 0;
                    stable_since = Instant::now();
                    log(
                        LogLevel::Debug,
                        "direct-computing::stream-viewer",
                        &format!(
                            "no complete video frame for 2s; requested keyframe bitrate={} fps={} incomplete_datagrams={}",
                            requested_bitrate, requested_fps, incomplete_datagrams
                        ),
                    );
                }
                if stable_inter_frames >= RATE_UPGRADE_MIN_INTER_FRAMES
                    && stable_since.elapsed() >= RATE_UPGRADE_INTERVAL
                    && (requested_bitrate < profile.max_bitrate
                        || u32::from(requested_fps) < profile.max_fps)
                {
                    (requested_bitrate, requested_fps) = increase_stream_rate(
                        requested_bitrate,
                        requested_fps,
                        profile.max_bitrate,
                        profile.max_fps,
                    );
                    control
                        .send(&WireMessage::RateHint {
                            bitrate: requested_bitrate,
                            frames_per_second: requested_fps,
                        })
                        .await?;
                    stable_inter_frames = 0;
                    stable_since = Instant::now();
                    log(
                        LogLevel::Info,
                        "direct-computing::stream-viewer",
                        &format!(
                            "stable network; increased stream rate to bitrate={} fps={}",
                            requested_bitrate, requested_fps
                        ),
                    );
                }
                sink.pump_events();
                if let Some((width, height)) = desktop_size {
                    // Pointer motion is state, not a queue. Coalesce multiple
                    // moves observed during one UI tick while preserving key
                    // transitions in order.
                    let mut latest_pointer = None;
                    for event in sink.drain_input_events() {
                        match event {
                            dc_protocol::InputEvent::Pointer { x, y, buttons } => {
                                latest_pointer = Some(dc_protocol::InputEvent::Pointer {
                                    x,
                                    y,
                                    buttons,
                                });
                            }
                            event => {
                                validate_input(&event, width, height)?;
                                control.send(&WireMessage::Input(event)).await?;
                            }
                        }
                    }
                    if let Some(event) = latest_pointer {
                        validate_input(&event, width, height)?;
                        control.send(&WireMessage::Input(event)).await?;
                    }
                }
                tokio::time::sleep(Duration::from_millis(4)).await;
                if report_started.elapsed() >= Duration::from_secs(1) {
                    log(
                        LogLevel::Info,
                        "direct-computing::stream-viewer",
                        &format!(
                            "presented={} dropped={} decode_avg={:.2}ms present_avg={:.2}ms receive_to_present_avg={:.2}ms",
                            presented,
                            dropped,
                            average_millis(decode_total, presented),
                            average_millis(present_total, presented),
                            average_millis(receive_to_present_total, presented)
                        ),
                    );
                    presented = 0;
                    dropped = 0;
                    decode_total = Duration::ZERO;
                    present_total = Duration::ZERO;
                    receive_to_present_total = Duration::ZERO;
                    report_started = Instant::now();
                }
            }
            let _ = control.send(&WireMessage::Close).await;
            Ok(())
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use dc_media::FrameLayout;

    #[test]
    fn rate_reduction_reaches_but_does_not_cross_floor() {
        assert_eq!(reduce_stream_rate(320_000, 8, 96_000, 4), (213_333, 5));
        assert_eq!(reduce_stream_rate(160_000, 5, 64_000, 3), (106_666, 3));
        assert_eq!(reduce_stream_rate(64_000, 3, 64_000, 3), (64_000, 3));
        assert_eq!(increase_stream_rate(96_000, 4, 320_000, 8), (128_001, 5));
        assert_eq!(increase_stream_rate(320_000, 8, 320_000, 8), (320_000, 8));
    }

    #[test]
    fn stream_control_clamps_untrusted_rate_hints() {
        let control = StreamControl::new(DEFAULT_RATE_PROFILE);
        control.update_rate(1, 1);
        assert_eq!(
            control.target_bitrate.load(Ordering::Acquire),
            DEFAULT_RATE_PROFILE.min_bitrate
        );
        assert_eq!(
            control.target_fps.load(Ordering::Acquire),
            DEFAULT_RATE_PROFILE.min_fps
        );
        control.update_rate(u32::MAX, u16::MAX);
        assert_eq!(
            control.target_bitrate.load(Ordering::Acquire),
            DEFAULT_RATE_PROFILE.max_bitrate
        );
        assert_eq!(
            control.target_fps.load(Ordering::Acquire),
            DEFAULT_RATE_PROFILE.max_fps
        );
    }

    #[test]
    fn oversized_frame_at_rate_floor_does_not_start_recovery_loop() {
        let control = StreamControl::new(DEFAULT_RATE_PROFILE);
        control.update_rate(
            DEFAULT_RATE_PROFILE.min_bitrate,
            DEFAULT_RATE_PROFILE.min_fps as u16,
        );
        assert_eq!(
            control.request_lower_rate_keyframe(),
            None,
            "a floor-rate stream must not repeatedly queue identical recovery keyframes"
        );
        assert!(!control.recovery_pending.load(Ordering::Acquire));
        assert!(!control.keyframe_requested.load(Ordering::Acquire));
    }

    #[test]
    fn startup_zero_frame_detection_allows_real_content() {
        let size = FrameSize::new(2, 2).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        let zero = VideoFrame::new(0, Duration::ZERO, layout, vec![0; 16]).unwrap();
        let mut content_bytes = vec![0; 16];
        content_bytes[4] = 5;
        let content = VideoFrame::new(1, Duration::ZERO, layout, content_bytes).unwrap();
        assert!(is_effectively_zero_frame(&zero));
        assert!(!is_effectively_zero_frame(&content));
    }

    #[test]
    fn color_depth_reduction_preserves_frame_geometry() {
        let size = FrameSize::new(2, 1).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        let frame = VideoFrame::new(
            7,
            Duration::ZERO,
            layout,
            vec![0x13, 0x27, 0x3b, 0xff, 0x45, 0x59, 0x6d, 0xff],
        )
        .unwrap();
        let reduced = prepare_network_frame(frame, DEFAULT_RATE_PROFILE).unwrap();
        assert_eq!(reduced.layout().size(), size);
        assert_eq!(
            reduced.data(),
            &[0x10, 0x24, 0x38, 0xff, 0x40, 0x58, 0x68, 0xff]
        );
    }

    #[test]
    fn keyframe_recovery_requests_are_coalesced_until_acknowledged() {
        let control = StreamControl::new(DEFAULT_RATE_PROFILE);
        assert!(control.request_keyframe());
        assert!(!control.request_keyframe());
        control.acknowledge_keyframe();
        assert!(control.request_keyframe());
    }
}

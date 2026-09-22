use dc_auth::{PasswordVerifier, Permissions};
use dc_common::{init_logging, log, DcError, LogLevel, Result};
use dc_desktop::validate_input;
use dc_desktop::{packet_to_message, packetize_video_message, VideoDatagramReassembler};
#[cfg(target_os = "windows")]
use dc_media::{write_bmp, LastFrameSink};
use dc_media::{
    ChecksumSink, FrameSize, LoopbackPipeline, OpenH264Decoder, OpenH264Encoder,
    SyntheticFrameSource,
};
#[cfg(target_os = "macos")]
use dc_platform::VideoToolboxH264Decoder;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct EncodedFrame {
    message: Arc<WireMessage>,
    sequence: u64,
    encoded_bytes: u64,
    capture_time: Duration,
    encode_time: Duration,
}

#[derive(Clone)]
struct ReceivedFrame {
    message: Arc<WireMessage>,
    received_at: Instant,
}

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
            run_network_host(&address, &password)
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
            run_network_viewer(&address, &password, fingerprint.as_deref())
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
    "usage: direct-computing [--host [addr] <password> | --connect <addr> <password> [cert-sha256] | --loopback [frame-count] | --window-test [duration-seconds] | --capture-test [frame-count] [display-index] [output.bmp] | --preview [display-index] [duration-seconds]]"
}

fn run_network_host(address: &str, password: &str) -> Result<()> {
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
            authenticate_server(
                &mut control,
                &verifier,
                permissions,
                Capabilities {
                    desktop: true,
                    control_input: true,
                    ..Capabilities::default()
                },
            )
            .await?;
            control.send(&WireMessage::OpenDesktop).await?;
            let keyframe_requested = Arc::new(AtomicBool::new(false));
            let input_keyframe_requested = keyframe_requested.clone();
            tokio::spawn(async move {
                if let Err(error) = receive_host_input(&mut control, input_keyframe_requested).await
                {
                    log(
                        LogLevel::Warn,
                        "direct-computing::host",
                        &format!("input stream closed: {error}"),
                    );
                }
            });
            let result = send_desktop_frames(&connection, keyframe_requested).await;
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
    keyframe_requested: Arc<AtomicBool>,
) -> Result<()> {
    let mut injector = WindowsInputInjector::new();
    loop {
        match stream.receive().await? {
            WireMessage::Input(event) => injector.inject(&event)?,
            WireMessage::KeyframeRequest { .. } => {
                keyframe_requested.store(true, Ordering::Release);
            }
            WireMessage::Close => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn receive_host_input(
    stream: &mut dc_transport::FramedStream,
    keyframe_requested: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match stream.receive().await? {
            WireMessage::Input(_) => {
                return Err(DcError::Unsupported(
                    "remote input injection requires Windows Host".into(),
                ));
            }
            WireMessage::KeyframeRequest { .. } => {
                keyframe_requested.store(true, Ordering::Release);
            }
            WireMessage::Close => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(target_os = "windows")]
async fn send_desktop_frames(
    connection: &QuicConnection,
    keyframe_requested: Arc<AtomicBool>,
) -> Result<()> {
    let source = WindowsDesktopCapturer::new(0, 1_000)?;
    send_frames_from(connection, keyframe_requested, source, |frame| {
        let size = frame.layout().size();
        match WindowsMediaFoundationH264Encoder::new(size.width(), size.height(), 8_000_000, 30) {
            Ok(encoder) => Ok(Box::new(encoder)),
            Err(error) => {
                log(
                    LogLevel::Warn,
                    "direct-computing::stream-host",
                    &format!("Media Foundation unavailable, using OpenH264: {error}"),
                );
                Ok(Box::new(OpenH264Encoder::new(2_000_000, 30.0)?))
            }
        }
    })
    .await
}

#[cfg(not(target_os = "windows"))]
async fn send_desktop_frames(
    connection: &QuicConnection,
    keyframe_requested: Arc<AtomicBool>,
) -> Result<()> {
    let size = FrameSize::new(640, 360)?;
    send_frames_from(
        connection,
        keyframe_requested,
        SyntheticFrameSource::new(size, 30)?,
        |_| Ok(Box::new(OpenH264Encoder::new(2_000_000, 30.0)?)),
    )
    .await
}

async fn send_frames_from<S, F>(
    connection: &QuicConnection,
    keyframe_requested: Arc<AtomicBool>,
    mut source: S,
    create_encoder: F,
) -> Result<()>
where
    S: dc_media::FrameSource + Send + 'static,
    F: Fn(&dc_media::VideoFrame) -> Result<Box<dyn dc_media::VideoEncoder>> + Send + 'static,
{
    let max_datagram_size = connection.max_datagram_size().ok_or_else(|| {
        DcError::Unsupported("peer did not negotiate QUIC DATAGRAM support".into())
    })?;
    // Keep capture and encoding off the async runtime thread. The bounded
    // producer queue protects the runtime, while the datagram lane allows the
    // transport to discard stale media instead of blocking control traffic.
    let (packet_tx, mut packet_rx) = tokio::sync::mpsc::channel::<Arc<EncodedFrame>>(4);
    let producer = tokio::task::spawn_blocking(move || -> Result<()> {
        use dc_media::EncodeOutcome;
        let first_capture_started = Instant::now();
        let first_frame = loop {
            match source.capture() {
                Err(DcError::Timeout(_)) => continue,
                result => break result?,
            }
        };
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
                "first captured frame sequence={} sample_sum={} non_zero={} dimensions={}x{}",
                pending.0.sequence(),
                first_source_sample,
                first_source_non_zero,
                pending.0.layout().size().width(),
                pending.0.layout().size().height()
            ),
        );
        let mut encoder = create_encoder(&pending.0)?;
        let mut last_frame;
        let mut last_sequence;
        let capabilities = encoder.capabilities();
        log(
            LogLevel::Info,
            "direct-computing::stream-host",
            &format!(
                "encoder backend={} hardware={} zero_copy={} low_latency={}",
                capabilities.backend,
                capabilities.hardware_accelerated,
                capabilities.zero_copy_input,
                capabilities.low_latency
            ),
        );
        let frame_interval = Duration::from_secs_f64(1.0 / 30.0);
        let mut next_frame_at = std::time::Instant::now();
        let mut report_started = Instant::now();
        let mut captured = 0_u64;
        let mut encoded = 0_u64;
        let mut capture_total = Duration::ZERO;
        let mut encode_total = Duration::ZERO;
        loop {
            let (frame, capture_time) = pending;
            captured += 1;
            capture_total += capture_time;
            let recovery_frame = frame.clone();
            last_sequence = frame.sequence();
            if keyframe_requested.swap(false, Ordering::AcqRel) {
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
                    encoder = create_encoder(&frame)?;
                }
            }
            let encode_started = Instant::now();
            if let EncodeOutcome::Packet(packet) = encoder.encode(frame)? {
                let encode_time = encode_started.elapsed();
                encode_total += encode_time;
                encoded += 1;
                if packet_tx
                    .blocking_send(Arc::new(EncodedFrame {
                        sequence: packet.sequence(),
                        encoded_bytes: packet.data().len() as u64,
                        capture_time,
                        encode_time,
                        message: Arc::new(packet_to_message(&packet)?),
                    }))
                    .is_err()
                {
                    return Ok(());
                }
            }
            last_frame = Some(recovery_frame);
            if report_started.elapsed() >= Duration::from_secs(1) {
                log(
                    LogLevel::Info,
                    "direct-computing::stream-host",
                    &format!(
                        "capture={} encoded={} capture_avg={:.2}ms encode_avg={:.2}ms",
                        captured,
                        encoded,
                        average_millis(capture_total, captured),
                        average_millis(encode_total, encoded)
                    ),
                );
                captured = 0;
                encoded = 0;
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
            let capture_started = Instant::now();
            pending = loop {
                match source.capture() {
                    Err(DcError::Timeout(_)) if keyframe_requested.load(Ordering::Acquire) => {
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
                    Err(DcError::Timeout(_)) => continue,
                    result => break (result?, capture_started.elapsed()),
                }
            };
        }
    });

    let mut sent = 0_u64;
    let mut datagrams_sent = 0_u64;
    let mut first_frame_logged = false;
    let mut send_total = Duration::ZERO;
    let mut send_report_started = Instant::now();
    while let Some(frame) = packet_rx.recv().await {
        let send_started = Instant::now();
        let fragments = packetize_video_message(frame.message.as_ref(), max_datagram_size)?;
        let keyframe = matches!(
            frame.message.as_ref(),
            WireMessage::Video { keyframe: true, .. }
        );
        // A keyframe is required before inter frames are useful.  Repeat its
        // fragments once so a single lost QUIC DATAGRAM does not leave the
        // viewer waiting for another GOP.  Inter frames remain single-shot to
        // keep bandwidth and queue pressure bounded on narrow links.
        let fragment_repetitions = if keyframe { 2 } else { 1 };
        if !first_frame_logged {
            log(
                LogLevel::Info,
                "direct-computing::stream-host",
                &format!(
                    "first video frame sequence={} fragments={} datagram_size={} keyframe={} repetitions={}",
                    frame.sequence,
                    fragments.len(),
                    max_datagram_size,
                    keyframe,
                    fragment_repetitions
                ),
            );
            first_frame_logged = true;
        }
        for fragment in fragments {
            for _ in 0..fragment_repetitions {
                // Datagram sends are intentionally non-blocking. Quinn discards
                // queued old datagrams when its bounded media buffer is full,
                // preventing stale frames from delaying input/control streams.
                connection.send_datagram(fragment.clone().into())?;
                datagrams_sent = datagrams_sent.saturating_add(1);
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
                &format!("video datagrams sent={datagrams_sent}"),
            );
            sent = 0;
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
        WireMessage::Video { sequence, .. } => Some(*sequence),
        _ => None,
    }
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
                    log(
                        LogLevel::Warn,
                        "direct-computing::stream-viewer",
                        &format!("VideoToolbox decode failed, switching to OpenH264: {error}"),
                    );
                    self.hardware = None;
                }
            }
        }
        dc_media::VideoDecoder::decode(&mut self.fallback, packet)
    }
}

#[cfg(target_os = "windows")]
struct WindowsViewerDecoder {
    hardware: Option<WindowsMediaFoundationH264Decoder>,
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
        if self.hardware.is_none() {
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

fn run_network_viewer(address: &str, password: &str, fingerprint: Option<&str>) -> Result<()> {
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
                    ..Capabilities::default()
                },
            )
            .await?;
            if !session.permissions.view_desktop {
                return Err(DcError::Unsupported(
                    "desktop viewing is not permitted".into(),
                ));
            }
            let _ = control.receive().await?;
            let max_datagram_size = connection.max_datagram_size();
            log(
                LogLevel::Info,
                "direct-computing::stream-viewer",
                &format!(
                    "video transport=quic-datagram max_datagram_size={:?}",
                    max_datagram_size
                ),
            );
            if max_datagram_size.is_none() {
                return Err(DcError::Unsupported(
                    "peer does not support QUIC DATAGRAM video; update the Host and reconnect"
                        .into(),
                ));
            }
            let mut decoder = create_viewer_decoder()?;
            let mut sink = PreviewWindowSink::new("Direct Computing - Remote Desktop");
            sink.show_placeholder(640, 360)?;
            log(
                LogLevel::Info,
                "direct-computing::stream-viewer",
                &format!("present backend={}", PreviewWindowSink::render_backend()),
            );
            let mut desktop_size = None;
            // Video is carried over an unreliable, unordered QUIC DATAGRAM
            // lane. Incomplete or stale frames are discarded by the
            // reassembler; control/input continue using the reliable stream.
            let (video_tx, mut video_rx) = tokio::sync::watch::channel::<
                Option<std::result::Result<ReceivedFrame, String>>,
            >(None);
            let datagram_connection = connection.clone();
            tokio::spawn(async move {
                let mut reassembler = VideoDatagramReassembler::new();
                let mut datagrams_received = 0_u64;
                loop {
                    let received_at = Instant::now();
                    match datagram_connection.receive_datagram().await {
                        Ok(datagram) => {
                            datagrams_received += 1;
                            if datagrams_received <= 3 {
                                log(
                                    LogLevel::Info,
                                    "direct-computing::stream-viewer",
                                    &format!(
                                        "received video datagram index={} bytes={}",
                                        datagrams_received,
                                        datagram.len()
                                    ),
                                );
                            }
                            match reassembler.push(&datagram) {
                            Ok(Some(message)) => {
                                log(
                                    LogLevel::Info,
                                    "direct-computing::stream-viewer",
                                    &format!(
                                        "received complete video frame after {} datagrams",
                                        datagrams_received
                                    ),
                                );
                                if video_tx
                                    .send(Some(Ok(ReceivedFrame {
                                        message: Arc::new(message),
                                        received_at,
                                    })))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Ok(None) => continue,
                            Err(error) => {
                                log(
                                    LogLevel::Warn,
                                    "direct-computing::stream-viewer",
                                    &format!("video datagram rejected: {error}"),
                                );
                                let _ = video_tx.send(Some(Err(error.to_string())));
                                break;
                            }
                            }
                        }
                        Err(error) => {
                            let _ = video_tx.send(Some(Err(error.to_string())));
                            break;
                        }
                    }
                }
            });
            let mut last_sequence: Option<u64> = None;
            let mut last_keyframe_request: Option<Instant> = None;
            let mut last_frame_progress = Instant::now();
            let mut packet_logged = false;
            let mut decoded_logged = false;
            let mut presented = 0_u64;
            let mut dropped = 0_u64;
            let mut decode_total = Duration::ZERO;
            let mut present_total = Duration::ZERO;
            let mut receive_to_present_total = Duration::ZERO;
            let mut report_started = Instant::now();
            while sink.is_open() {
                let update = match tokio::time::timeout(
                    Duration::from_millis(16),
                    video_rx.changed(),
                )
                .await
                {
                    Ok(Ok(())) => video_rx.borrow_and_update().clone(),
                    Ok(Err(_)) => {
                        return Err(DcError::Platform(
                            "video datagram receiver closed".into(),
                        ));
                    }
                    Err(_) => None,
                };
                if let Some(update) = update {
                    last_frame_progress = Instant::now();
                    let received = match update {
                        Ok(received) => received,
                        Err(error) => {
                            return Err(DcError::Platform(format!(
                                "video stream closed: {error}"
                            )));
                        }
                    };
                    let sequence = video_sequence(received.message.as_ref());
                    if let (Some(previous), Some(current)) = (last_sequence, sequence) {
                        let gap = current.saturating_sub(previous.saturating_add(1));
                        dropped += gap;
                        if gap > 0
                            && last_keyframe_request
                                .is_none_or(|time| time.elapsed() >= Duration::from_secs(2))
                        {
                            control
                                .send(&WireMessage::KeyframeRequest {
                                    last_sequence: previous,
                                })
                                .await?;
                            last_keyframe_request = Some(Instant::now());
                        }
                    }
                    last_sequence = sequence;
                    let decode_started = Instant::now();
                    let packet = dc_desktop::message_to_packet_ref(received.message.as_ref())?;
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
                    let frame = match decoder.decode(packet) {
                        Ok(frame) => frame,
                        Err(error) => {
                            if last_keyframe_request
                                .is_none_or(|time| time.elapsed() >= Duration::from_secs(2))
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
                    if !decoded_logged {
                        let sample = frame
                            .data()
                            .iter()
                            .take(64)
                            .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
                        let non_zero = frame.data().iter().filter(|byte| **byte > 4).count();
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
                    decode_total += decode_started.elapsed();
                    let present_started = Instant::now();
                    dc_media::FrameSink::present(&mut sink, frame)?;
                    present_total += present_started.elapsed();
                    receive_to_present_total += received.received_at.elapsed();
                    presented += 1;
                }
                if last_frame_progress.elapsed() >= Duration::from_secs(1)
                    && last_keyframe_request
                        .is_none_or(|time| time.elapsed() >= Duration::from_secs(2))
                {
                    control
                        .send(&WireMessage::KeyframeRequest {
                            last_sequence: last_sequence.unwrap_or_default(),
                        })
                        .await?;
                    last_keyframe_request = Some(Instant::now());
                    log(
                        LogLevel::Debug,
                        "direct-computing::stream-viewer",
                        "no complete video frame received for 1s; requested keyframe",
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

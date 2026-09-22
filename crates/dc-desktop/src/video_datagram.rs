use dc_common::{DcError, Result};
use dc_protocol::{WireMessage, MAX_MESSAGE_SIZE};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = *b"DCVD";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 8 + 2 + 2 + 4;
const MAX_IN_FLIGHT: usize = 8;
const REASSEMBLY_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartialFrame {
    total_len: usize,
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
    created: Instant,
}

/// Counters used to observe whether the unreliable video lane is dropping
/// stale data as intended rather than building an unbounded queue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoDatagramStats {
    pub completed_frames: u64,
    pub incomplete_frames: u64,
    pub malformed_fragments: u64,
}

/// Reassembles video messages from unordered QUIC DATAGRAM fragments. A frame
/// that misses any fragment before the deadline is discarded; the next frame
/// can still be decoded without waiting for it.
pub struct VideoDatagramReassembler {
    frames: BTreeMap<u64, PartialFrame>,
    latest_completed: Option<u64>,
    stats: VideoDatagramStats,
}

impl Default for VideoDatagramReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDatagramReassembler {
    pub fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            latest_completed: None,
            stats: VideoDatagramStats::default(),
        }
    }

    pub fn stats(&self) -> VideoDatagramStats {
        self.stats
    }

    pub fn push(&mut self, datagram: &[u8]) -> Result<Option<WireMessage>> {
        self.expire();
        let parsed = match parse_fragment(datagram) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
                return Err(error);
            }
        };
        let (sequence, index, count, total_len, payload) = parsed;
        if self
            .latest_completed
            .is_some_and(|latest| sequence <= latest)
        {
            return Ok(None);
        }
        if count == 0 || index >= count || total_len == 0 || total_len > MAX_MESSAGE_SIZE {
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec(
                "invalid video datagram fragment metadata".into(),
            ));
        }
        if self.frames.len() >= MAX_IN_FLIGHT && !self.frames.contains_key(&sequence) {
            if let Some(oldest) = self.frames.keys().next().copied() {
                self.frames.remove(&oldest);
                self.stats.incomplete_frames = self.stats.incomplete_frames.saturating_add(1);
            }
        }
        let frame = self.frames.entry(sequence).or_insert_with(|| PartialFrame {
            total_len,
            fragments: vec![None; count as usize],
            received: 0,
            created: Instant::now(),
        });
        if frame.total_len != total_len || frame.fragments.len() != count as usize {
            self.frames.remove(&sequence);
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec(
                "video datagram fragment metadata changed".into(),
            ));
        }
        if frame.fragments[index as usize].is_none() {
            frame.fragments[index as usize] = Some(payload);
            frame.received += 1;
        }
        if frame.received != frame.fragments.len() {
            return Ok(None);
        }
        let frame = self.frames.remove(&sequence).expect("frame entry exists");
        let mut encoded = Vec::with_capacity(frame.total_len);
        for fragment in frame.fragments {
            encoded.extend(fragment.expect("complete frame has every fragment"));
        }
        if encoded.len() != frame.total_len {
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec(
                "video datagram frame length mismatch".into(),
            ));
        }
        let message = WireMessage::decode(&encoded)?;
        if !matches!(message, WireMessage::Video { sequence: value, .. } if value == sequence) {
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec("video datagram sequence mismatch".into()));
        }
        self.latest_completed = Some(sequence);
        self.stats.completed_frames = self.stats.completed_frames.saturating_add(1);
        Ok(Some(message))
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<u64> = self
            .frames
            .iter()
            .filter_map(|(sequence, frame)| {
                (now.duration_since(frame.created) > REASSEMBLY_TIMEOUT).then_some(*sequence)
            })
            .collect();
        for sequence in expired {
            self.frames.remove(&sequence);
            self.stats.incomplete_frames = self.stats.incomplete_frames.saturating_add(1);
        }
    }
}

/// Serialize one encoded video message into MTU-sized unreliable fragments.
pub fn packetize_video_message(
    message: &WireMessage,
    max_datagram_size: usize,
) -> Result<Vec<Vec<u8>>> {
    let sequence = match message {
        WireMessage::Video { sequence, .. } => *sequence,
        _ => return Err(DcError::InvalidInput("expected video message".into())),
    };
    if max_datagram_size <= HEADER_LEN {
        return Err(DcError::InvalidInput(
            "QUIC datagram MTU is too small".into(),
        ));
    }
    let encoded = message.encode()?;
    let payload_size = max_datagram_size - HEADER_LEN;
    let count = encoded.len().div_ceil(payload_size);
    let count = u16::try_from(count)
        .map_err(|_| DcError::InvalidInput("video message requires too many datagrams".into()))?;
    let total_len = u32::try_from(encoded.len())
        .map_err(|_| DcError::InvalidInput("video message is too large".into()))?;
    let mut output = Vec::with_capacity(count as usize);
    for index in 0..count {
        let start = usize::from(index) * payload_size;
        let end = (start + payload_size).min(encoded.len());
        let payload = &encoded[start..end];
        let mut fragment = Vec::with_capacity(HEADER_LEN + payload.len());
        fragment.extend_from_slice(&MAGIC);
        fragment.push(VERSION);
        fragment.extend_from_slice(&sequence.to_be_bytes());
        fragment.extend_from_slice(&index.to_be_bytes());
        fragment.extend_from_slice(&count.to_be_bytes());
        fragment.extend_from_slice(&total_len.to_be_bytes());
        fragment.extend_from_slice(payload);
        output.push(fragment);
    }
    Ok(output)
}

fn parse_fragment(datagram: &[u8]) -> Result<(u64, u16, u16, usize, Vec<u8>)> {
    if datagram.len() <= HEADER_LEN || datagram[..4] != MAGIC || datagram[4] != VERSION {
        return Err(DcError::Codec("invalid video datagram header".into()));
    }
    let sequence = u64::from_be_bytes(datagram[5..13].try_into().unwrap());
    let index = u16::from_be_bytes(datagram[13..15].try_into().unwrap());
    let count = u16::from_be_bytes(datagram[15..17].try_into().unwrap());
    let total_len = u32::from_be_bytes(datagram[17..21].try_into().unwrap()) as usize;
    Ok((
        sequence,
        index,
        count,
        total_len,
        datagram[HEADER_LEN..].to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_protocol::WireMessage;

    fn message(payload_len: usize) -> WireMessage {
        WireMessage::Video {
            codec: 1,
            sequence: 7,
            timestamp_millis: 42,
            keyframe: true,
            width: 640,
            height: 360,
            pixel_format: 1,
            stride: 2_560,
            data: vec![0x5a; payload_len],
        }
    }

    #[test]
    fn packetizes_and_reassembles_out_of_order() {
        let fragments = packetize_video_message(&message(4_000), 600).unwrap();
        assert!(fragments.len() > 1);
        let mut reassembler = VideoDatagramReassembler::new();
        let mut result = None;
        for fragment in fragments.iter().rev() {
            result = reassembler.push(fragment).unwrap().or(result);
        }
        assert_eq!(result, Some(message(4_000)));
        assert_eq!(reassembler.stats().completed_frames, 1);
    }

    #[test]
    fn incomplete_frame_is_not_delivered() {
        let fragments = packetize_video_message(&message(2_000), 500).unwrap();
        let mut reassembler = VideoDatagramReassembler::new();
        assert!(reassembler.push(&fragments[0]).unwrap().is_none());
        assert_eq!(reassembler.stats().completed_frames, 0);
    }

    #[test]
    fn encoded_frame_survives_datagram_path_and_decodes() {
        use dc_media::{
            EncodeOutcome, FrameSource, OpenH264Decoder, OpenH264Encoder, VideoDecoder,
            VideoEncoder,
        };

        let size = crate::FrameSize::new(320, 180).unwrap();
        let mut source = dc_media::SyntheticFrameSource::new(size, 30).unwrap();
        let frame = source.capture().unwrap();
        let mut encoder = OpenH264Encoder::new(1_000_000, 30.0).unwrap();
        let packet = match encoder.encode(frame).unwrap() {
            EncodeOutcome::Packet(packet) => packet,
            EncodeOutcome::Skipped => panic!("synthetic frame unexpectedly skipped"),
        };
        let message = crate::packet_to_message(&packet).unwrap();
        let fragments = packetize_video_message(&message, 1_200).unwrap();
        let mut reassembler = VideoDatagramReassembler::new();
        let mut decoded_message = None;
        for fragment in fragments {
            decoded_message = reassembler.push(&fragment).unwrap().or(decoded_message);
        }
        let decoded_packet = crate::message_to_packet(decoded_message.unwrap()).unwrap();
        let mut decoder = OpenH264Decoder::new().unwrap();
        let decoded = decoder.decode(decoded_packet).unwrap();
        assert_eq!(decoded.layout().size(), size);
        assert!(decoded.data().iter().any(|byte| *byte != 0));
    }
}

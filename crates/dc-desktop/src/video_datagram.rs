use dc_common::{DcError, Result};
use dc_media::split_h264_nal_units;
use dc_protocol::{WireMessage, MAX_MESSAGE_SIZE};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = *b"DCVD";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 8 + 2 + 2 + 4;
const MAX_IN_FLIGHT: usize = 8;
const REASSEMBLY_TIMEOUT: Duration = Duration::from_millis(500);
const NAL_MAGIC: [u8; 4] = *b"DCVN";
const NAL_VERSION: u8 = 1;
const NAL_HEADER_LEN: usize = 48;
const MAX_NAL_UNITS: usize = 4_096;
const MAX_NAL_FRAGMENTS: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoDatagramKind {
    Legacy,
    H264Nal,
}

/// Identify the envelope before selecting a reassembler. NAL capability
/// negotiation does not change the envelope used by desktop-region updates.
pub fn classify_video_datagram(datagram: &[u8]) -> Option<VideoDatagramKind> {
    if datagram.len() >= 5 && datagram[..4] == NAL_MAGIC && datagram[4] == NAL_VERSION {
        Some(VideoDatagramKind::H264Nal)
    } else if datagram.len() >= 5 && datagram[..4] == MAGIC && datagram[4] == VERSION {
        Some(VideoDatagramKind::Legacy)
    } else {
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartialNalUnit {
    total_len: usize,
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartialNalFrame {
    codec: u8,
    timestamp_millis: u64,
    keyframe: bool,
    width: u32,
    height: u32,
    pixel_format: u8,
    stride: u32,
    units: Vec<Option<Vec<u8>>>,
    partial_units: BTreeMap<u16, PartialNalUnit>,
    last_fragment: Instant,
}

/// Reassembles independently packetized H.264 NAL units. Each completed unit
/// is retained immediately, so later units do not depend on one monolithic
/// frame-fragment sequence. The current decoder API still emits a picture only
/// after all NAL units in the access unit have arrived.
pub struct H264NalDatagramReassembler {
    frames: BTreeMap<u64, PartialNalFrame>,
    latest_completed: Option<u64>,
    stats: VideoDatagramStats,
}

impl Default for H264NalDatagramReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl H264NalDatagramReassembler {
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
        let fragment = match parse_nal_fragment(datagram) {
            Ok(fragment) => fragment,
            Err(error) => {
                self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
                return Err(error);
            }
        };
        if self
            .latest_completed
            .is_some_and(|latest| fragment.sequence <= latest)
        {
            return Ok(None);
        }
        if fragment.unit_count == 0
            || fragment.codec != 1
            || usize::from(fragment.unit_count) > MAX_NAL_UNITS
            || fragment.unit_index >= fragment.unit_count
            || fragment.fragment_count == 0
            || usize::from(fragment.fragment_count) > MAX_NAL_FRAGMENTS
            || fragment.fragment_index >= fragment.fragment_count
            || fragment.unit_len == 0
            || fragment.unit_len > MAX_MESSAGE_SIZE
            || fragment.payload.len() > fragment.unit_len
        {
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec("invalid H.264 NAL fragment metadata".into()));
        }
        if self.frames.len() >= MAX_IN_FLIGHT && !self.frames.contains_key(&fragment.sequence) {
            if let Some(oldest) = self.frames.keys().next().copied() {
                self.frames.remove(&oldest);
                self.stats.incomplete_frames = self.stats.incomplete_frames.saturating_add(1);
            }
        }
        let frame = self
            .frames
            .entry(fragment.sequence)
            .or_insert_with(|| PartialNalFrame {
                codec: fragment.codec,
                timestamp_millis: fragment.timestamp_millis,
                keyframe: fragment.keyframe,
                width: fragment.width,
                height: fragment.height,
                pixel_format: fragment.pixel_format,
                stride: fragment.stride,
                units: vec![None; fragment.unit_count as usize],
                partial_units: BTreeMap::new(),
                last_fragment: Instant::now(),
            });
        if frame.codec != fragment.codec
            || frame.timestamp_millis != fragment.timestamp_millis
            || frame.keyframe != fragment.keyframe
            || frame.width != fragment.width
            || frame.height != fragment.height
            || frame.pixel_format != fragment.pixel_format
            || frame.stride != fragment.stride
            || frame.units.len() != fragment.unit_count as usize
        {
            self.frames.remove(&fragment.sequence);
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec("H.264 NAL frame metadata changed".into()));
        }
        frame.last_fragment = Instant::now();
        if frame.units[fragment.unit_index as usize].is_none() {
            if !frame.partial_units.contains_key(&fragment.unit_index) {
                let declared_bytes = frame
                    .units
                    .iter()
                    .filter_map(Option::as_ref)
                    .map(Vec::len)
                    .chain(frame.partial_units.values().map(|unit| unit.total_len))
                    .sum::<usize>();
                if declared_bytes
                    .checked_add(fragment.unit_len)
                    .is_none_or(|total| total > MAX_MESSAGE_SIZE)
                {
                    self.frames.remove(&fragment.sequence);
                    self.stats.malformed_fragments =
                        self.stats.malformed_fragments.saturating_add(1);
                    return Err(DcError::Codec(
                        "H.264 access unit exceeds protocol limit".into(),
                    ));
                }
            }
            let unit = frame
                .partial_units
                .entry(fragment.unit_index)
                .or_insert_with(|| PartialNalUnit {
                    total_len: fragment.unit_len,
                    fragments: vec![None; fragment.fragment_count as usize],
                    received: 0,
                });
            if unit.total_len != fragment.unit_len
                || unit.fragments.len() != fragment.fragment_count as usize
            {
                self.frames.remove(&fragment.sequence);
                self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
                return Err(DcError::Codec("H.264 NAL metadata changed".into()));
            }
            if unit.fragments[fragment.fragment_index as usize].is_none() {
                unit.fragments[fragment.fragment_index as usize] = Some(fragment.payload);
                unit.received += 1;
            }
            if unit.received == unit.fragments.len() {
                let mut bytes = Vec::with_capacity(unit.total_len);
                for part in &unit.fragments {
                    bytes.extend(part.as_ref().expect("complete NAL has every fragment"));
                }
                if bytes.len() != unit.total_len {
                    self.frames.remove(&fragment.sequence);
                    self.stats.malformed_fragments =
                        self.stats.malformed_fragments.saturating_add(1);
                    return Err(DcError::Codec("H.264 NAL length mismatch".into()));
                }
                frame.units[fragment.unit_index as usize] = Some(bytes);
                frame.partial_units.remove(&fragment.unit_index);
            }
        }
        if frame.units.iter().any(Option::is_none) {
            return Ok(None);
        }
        let frame = self
            .frames
            .remove(&fragment.sequence)
            .expect("frame exists");
        let mut data = Vec::new();
        for unit in frame.units {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend(unit.expect("complete frame has every NAL"));
        }
        if data.len() > MAX_MESSAGE_SIZE {
            self.stats.malformed_fragments = self.stats.malformed_fragments.saturating_add(1);
            return Err(DcError::Codec(
                "H.264 access unit exceeds protocol limit".into(),
            ));
        }
        self.latest_completed = Some(fragment.sequence);
        self.stats.completed_frames = self.stats.completed_frames.saturating_add(1);
        Ok(Some(WireMessage::Video {
            codec: frame.codec,
            sequence: fragment.sequence,
            timestamp_millis: frame.timestamp_millis,
            keyframe: frame.keyframe,
            width: frame.width,
            height: frame.height,
            pixel_format: frame.pixel_format,
            stride: frame.stride,
            data,
        }))
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .frames
            .iter()
            .filter_map(|(sequence, frame)| {
                (now.duration_since(frame.last_fragment) > REASSEMBLY_TIMEOUT).then_some(*sequence)
            })
            .collect();
        for sequence in expired {
            self.frames.remove(&sequence);
            self.stats.incomplete_frames = self.stats.incomplete_frames.saturating_add(1);
        }
    }
}

struct NalFragment {
    codec: u8,
    sequence: u64,
    timestamp_millis: u64,
    keyframe: bool,
    width: u32,
    height: u32,
    pixel_format: u8,
    stride: u32,
    unit_index: u16,
    unit_count: u16,
    fragment_index: u16,
    fragment_count: u16,
    unit_len: usize,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartialFrame {
    total_len: usize,
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
    last_fragment: Instant,
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
            last_fragment: Instant::now(),
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
        frame.last_fragment = Instant::now();
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
        if media_sequence(&message) != Some(sequence) {
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
                // A paced frame may legitimately take longer than the timeout
                // in total. Expire only after fragment delivery has stalled.
                (now.duration_since(frame.last_fragment) > REASSEMBLY_TIMEOUT).then_some(*sequence)
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
        WireMessage::Video { sequence, .. } | WireMessage::DesktopUpdate { sequence, .. } => {
            *sequence
        }
        _ => {
            return Err(DcError::InvalidInput(
                "expected desktop media message".into(),
            ))
        }
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

/// Packetize an H.264 access unit at NAL/slice boundaries. Small NAL units are
/// carried whole and large units are fragmented independently, following the
/// same basic model as RTP FU-A without requiring RTP as the outer transport.
pub fn packetize_h264_nal_message(
    message: &WireMessage,
    max_datagram_size: usize,
) -> Result<Vec<Vec<u8>>> {
    let WireMessage::Video {
        codec,
        sequence,
        timestamp_millis,
        keyframe,
        width,
        height,
        pixel_format,
        stride,
        data,
    } = message
    else {
        return Err(DcError::InvalidInput(
            "NAL packetization requires an H.264 video message".into(),
        ));
    };
    if *codec != 1 {
        return Err(DcError::InvalidInput(
            "NAL packetization only supports H.264".into(),
        ));
    }
    if max_datagram_size <= NAL_HEADER_LEN {
        return Err(DcError::InvalidInput(
            "QUIC datagram MTU is too small for NAL transport".into(),
        ));
    }
    let units = split_h264_nal_units(data)?;
    if units.is_empty() || units.len() > MAX_NAL_UNITS {
        return Err(DcError::Codec(
            "H.264 access unit has an invalid NAL count".into(),
        ));
    }
    let unit_count = u16::try_from(units.len())
        .map_err(|_| DcError::InvalidInput("H.264 access unit has too many NAL units".into()))?;
    let payload_size = max_datagram_size - NAL_HEADER_LEN;
    let mut output = Vec::new();
    for (unit_index, unit) in units.into_iter().enumerate() {
        let fragment_count = unit.len().div_ceil(payload_size);
        let fragment_count = u16::try_from(fragment_count)
            .map_err(|_| DcError::InvalidInput("H.264 NAL requires too many datagrams".into()))?;
        let unit_len = u32::try_from(unit.len())
            .map_err(|_| DcError::InvalidInput("H.264 NAL is too large".into()))?;
        for fragment_index in 0..fragment_count {
            let start = usize::from(fragment_index) * payload_size;
            let end = (start + payload_size).min(unit.len());
            let payload = &unit[start..end];
            let mut packet = Vec::with_capacity(NAL_HEADER_LEN + payload.len());
            packet.extend_from_slice(&NAL_MAGIC);
            packet.push(NAL_VERSION);
            packet.push(*codec);
            packet.extend_from_slice(&sequence.to_be_bytes());
            packet.extend_from_slice(&timestamp_millis.to_be_bytes());
            packet.push(u8::from(*keyframe));
            packet.extend_from_slice(&width.to_be_bytes());
            packet.extend_from_slice(&height.to_be_bytes());
            packet.push(*pixel_format);
            packet.extend_from_slice(&stride.to_be_bytes());
            packet.extend_from_slice(&(unit_index as u16).to_be_bytes());
            packet.extend_from_slice(&unit_count.to_be_bytes());
            packet.extend_from_slice(&fragment_index.to_be_bytes());
            packet.extend_from_slice(&fragment_count.to_be_bytes());
            packet.extend_from_slice(&unit_len.to_be_bytes());
            debug_assert_eq!(packet.len(), NAL_HEADER_LEN);
            packet.extend_from_slice(payload);
            output.push(packet);
        }
    }
    Ok(output)
}

fn media_sequence(message: &WireMessage) -> Option<u64> {
    match message {
        WireMessage::Video { sequence, .. } | WireMessage::DesktopUpdate { sequence, .. } => {
            Some(*sequence)
        }
        _ => None,
    }
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

fn parse_nal_fragment(datagram: &[u8]) -> Result<NalFragment> {
    if datagram.len() <= NAL_HEADER_LEN || datagram[..4] != NAL_MAGIC || datagram[4] != NAL_VERSION
    {
        return Err(DcError::Codec("invalid H.264 NAL datagram header".into()));
    }
    Ok(NalFragment {
        codec: datagram[5],
        sequence: u64::from_be_bytes(datagram[6..14].try_into().unwrap()),
        timestamp_millis: u64::from_be_bytes(datagram[14..22].try_into().unwrap()),
        keyframe: datagram[22] != 0,
        width: u32::from_be_bytes(datagram[23..27].try_into().unwrap()),
        height: u32::from_be_bytes(datagram[27..31].try_into().unwrap()),
        pixel_format: datagram[31],
        stride: u32::from_be_bytes(datagram[32..36].try_into().unwrap()),
        unit_index: u16::from_be_bytes(datagram[36..38].try_into().unwrap()),
        unit_count: u16::from_be_bytes(datagram[38..40].try_into().unwrap()),
        fragment_index: u16::from_be_bytes(datagram[40..42].try_into().unwrap()),
        fragment_count: u16::from_be_bytes(datagram[42..44].try_into().unwrap()),
        unit_len: u32::from_be_bytes(datagram[44..48].try_into().unwrap()) as usize,
        payload: datagram[NAL_HEADER_LEN..].to_vec(),
    })
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
    fn packetizes_h264_at_nal_boundaries_and_reassembles_out_of_order() {
        let message = WireMessage::Video {
            codec: 1,
            sequence: 9,
            timestamp_millis: 77,
            keyframe: false,
            width: 320,
            height: 180,
            pixel_format: 1,
            stride: 1_280,
            data: [
                &[0, 0, 0, 1, 0x41][..],
                &vec![0x11; 1_400],
                &[0, 0, 0, 1, 0x41][..],
                &vec![0x22; 900],
            ]
            .concat(),
        };
        let fragments = packetize_h264_nal_message(&message, 500).unwrap();
        assert!(fragments.len() > 2);
        assert!(fragments.iter().all(|fragment| fragment.len() <= 500));
        let mut reassembler = H264NalDatagramReassembler::new();
        let mut result = None;
        for fragment in fragments.iter().rev() {
            result = reassembler.push(fragment).unwrap().or(result);
        }
        assert_eq!(result, Some(message));
        assert_eq!(reassembler.stats().completed_frames, 1);
    }

    #[test]
    fn classifies_nal_and_legacy_datagram_envelopes() {
        let h264 = WireMessage::Video {
            codec: 1,
            sequence: 12,
            timestamp_millis: 42,
            keyframe: false,
            width: 64,
            height: 48,
            pixel_format: 1,
            stride: 256,
            data: [&[0, 0, 0, 1, 0x41][..], &[0x5a; 32]].concat(),
        };
        let desktop_update = WireMessage::DesktopUpdate {
            sequence: 13,
            timestamp_millis: 43,
            desktop_width: 64,
            desktop_height: 48,
            encoding: 0,
            regions: vec![dc_protocol::DesktopRect {
                x: 8,
                y: 8,
                width: 4,
                height: 4,
            }],
            data: vec![0x5a; 64],
        };
        let nal = packetize_h264_nal_message(&h264, 300).unwrap();
        let legacy = packetize_video_message(&desktop_update, 300).unwrap();

        assert_eq!(
            classify_video_datagram(&nal[0]),
            Some(VideoDatagramKind::H264Nal)
        );
        assert_eq!(
            classify_video_datagram(&legacy[0]),
            Some(VideoDatagramKind::Legacy)
        );
        assert_eq!(classify_video_datagram(b"bad"), None);
    }

    #[test]
    fn incomplete_nal_does_not_block_a_later_frame() {
        let first = WireMessage::Video {
            codec: 1,
            sequence: 10,
            timestamp_millis: 1,
            keyframe: false,
            width: 64,
            height: 48,
            pixel_format: 1,
            stride: 256,
            data: [&[0, 0, 0, 1, 0x41][..], &vec![1; 1_000]].concat(),
        };
        let second = WireMessage::Video {
            codec: 1,
            sequence: 11,
            timestamp_millis: 2,
            keyframe: false,
            width: 64,
            height: 48,
            pixel_format: 1,
            stride: 256,
            data: [&[0, 0, 0, 1, 0x41][..], &[2; 100]].concat(),
        };
        let first_fragments = packetize_h264_nal_message(&first, 300).unwrap();
        let second_fragments = packetize_h264_nal_message(&second, 300).unwrap();
        let mut reassembler = H264NalDatagramReassembler::new();
        assert!(reassembler.push(&first_fragments[0]).unwrap().is_none());
        let mut result = None;
        for fragment in second_fragments {
            result = reassembler.push(&fragment).unwrap().or(result);
        }
        assert_eq!(result, Some(second));
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

//! Detects in-band closed captions without decoding video.
//!
//! A pass-through origin already delivers CEA-608/708 captions: they travel as
//! SEI messages inside H.264 and HEVC access units and reach the output
//! segments untouched. What is missing is the *declaration* — a player surfaces
//! nothing unless the multivariant playlist says the captions are there. This
//! module answers only that question: does this track carry ATSC A53 caption
//! data, and on which channels.
//!
//! FFmpeg cannot answer it for us. `AV_PKT_DATA_A53_CC` is populated by capture
//! devices and encoders, never by a demuxer, and the extraction in
//! `libavcodec/h2645_sei.c` runs during *decode*. Decoding every packet to set
//! one manifest attribute is out of proportion for a pass-through node, so the
//! SEI is inspected directly instead. Nothing here rewrites the bitstream.

use broadcast_common::Parse;
use h264_reader::{
    avcc::AvcDecoderConfigurationRecord,
    nal::sei::{HeaderType, SeiReader, user_data_registered_itu_t_t35::ItuTT35},
    rbsp,
};

use crate::domain::{Codec, DiscoveredTrack};

/// SEI NAL unit type in H.264.
const NAL_UNIT_TYPE_SEI: u8 = 6;
/// HEVC prefix SEI (`PREFIX_SEI_NUT`).
const HEVC_NAL_PREFIX_SEI: u8 = 39;
/// HEVC suffix SEI (`SUFFIX_SEI_NUT`).
const HEVC_NAL_SUFFIX_SEI: u8 = 40;

/// ATSC provider code inside an ITU-T T.35 SEI payload.
const ATSC_PROVIDER_CODE: [u8; 2] = [0x00, 0x31];

/// ATSC A53 Part 4 user identifier.
const ATSC_USER_IDENTIFIER: &[u8; 4] = b"GA94";

/// `user_data_type_code` naming the `cc_data` structure.
const USER_DATA_TYPE_CC_DATA: u8 = 0x03;

/// Fixed bytes preceding the caption triplets: provider code, user identifier,
/// user data type code, the flags byte, and `em_data`.
const CC_DATA_HEADER_LEN: usize = 9;

/// Highest CEA-708 service number, and the widest a DTVCC packet can be.
///
/// Both come from CEA-708-E: service block headers name 1..=63, and a packet's
/// six-bit size code caps the bytes that follow at 127.
const MAXIMUM_DTVCC_SERVICE: u8 = 63;
const MAXIMUM_DTVCC_PACKET_BYTES: usize = 127;

/// Service block header value reserving the extended numbering escape.
const EXTENDED_SERVICE_ESCAPE: u8 = 0x07;

/// What a track was observed to carry.
///
/// Cumulative across the packets scanned so far, and deliberately separate from
/// what a playlist declares: reconciling the two is the caller's job, because
/// only the caller knows whether a rendition it has not yet observed is
/// evidence of a broken ladder or simply one that has not been reached.
///
/// Observations only accumulate; nothing here decays. A stream that carries
/// captions for ten seconds and then stops keeps its declaration for the rest
/// of the publication, so the playlist can out-claim the stream. That is the
/// deliberate trade: retracting a declaration mid-publication would remove a
/// caption group that clients have already selected, which is worse for a
/// viewer than an empty one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptionObservation {
    /// Whether a well-formed A53 caption payload was seen at all.
    pub a53_present: bool,
    /// CEA-608 Line 21 fields carrying data, as a bitmask of field index.
    pub cea608_fields: u8,
    /// CEA-708 DTVCC service numbers, as a bitmask indexed from service 1.
    ///
    /// Bit `n` means service `n + 1` was named by a service block header, so
    /// the widest permitted service — 63 — occupies the top bit. Only services
    /// actually identified appear here; null padding names none.
    pub cea708_services: u64,
    /// Whether DTVCC caption data was seen, independent of its service number.
    ///
    /// This is what makes a 708-only stream — the shape a transcoder emits when
    /// 608 carriage is disabled — distinguishable from no captions at all,
    /// even when no service block could be identified. Kept as a diagnostic
    /// rather than a declaration: a stream sending only null padding sets this
    /// and names no service, and declaring a caption track for padding would
    /// promise a service that carries nothing.
    pub dtvcc_present: bool,
}

impl CaptionObservation {
    fn merge(&mut self, other: Self) {
        self.a53_present |= other.a53_present;
        self.cea608_fields |= other.cea608_fields;
        self.cea708_services |= other.cea708_services;
        self.dtvcc_present |= other.dtvcc_present;
    }

    /// Records a service block's number, ignoring one outside 1..=63.
    fn observe_service(&mut self, service: u8) {
        if (1..=MAXIMUM_DTVCC_SERVICE).contains(&service) {
            self.cea708_services |= 1 << u32::from(service - 1);
        }
    }
}

/// Reassembles DTVCC packets from the `cc_data` triplet stream.
///
/// A packet is announced by a `cc_type` 3 triplet carrying its header, and
/// continues through `cc_type` 2 triplets until the byte count the header
/// promised is met. Bounded by construction: the six-bit size code caps a
/// packet at 127 bytes, so this can never grow with stream length.
///
/// Only as stateful as the wire demands. A packet is normally contained in one
/// `cc_data` payload, so this usually completes within a single access unit;
/// the buffer exists for the case where it does not.
#[derive(Debug, Default)]
struct DtvccAssembler {
    /// Bytes of the packet in progress, excluding its header.
    packet: Vec<u8>,
    /// Bytes still owed before the packet is complete.
    outstanding: usize,
}

impl DtvccAssembler {
    /// Begins a packet, returning the previous one if it was left unfinished.
    ///
    /// A new start before the outstanding count reached zero means bytes were
    /// lost — a dropped access unit, or a publisher that miscounted. The
    /// partial packet is discarded rather than parsed: a truncated service
    /// block would name a service from bytes that are not a header.
    fn begin(&mut self, header: u8) {
        self.packet.clear();
        // packet_size_code counts pairs, and the header itself is the first
        // byte of the packet, so the bytes still to come are one fewer.
        let size_code = usize::from(header & 0x3F);
        let total = if size_code == 0 {
            MAXIMUM_DTVCC_PACKET_BYTES + 1
        } else {
            size_code * 2
        };
        self.outstanding = total.saturating_sub(1);
    }

    /// Adds one byte of the packet in progress, if one is expected.
    fn push(&mut self, byte: u8) {
        if self.outstanding == 0 {
            return;
        }
        self.packet.push(byte);
        self.outstanding -= 1;
    }

    /// The completed packet, or `None` while bytes are still owed.
    fn complete(&self) -> Option<&[u8]> {
        (self.outstanding == 0 && !self.packet.is_empty()).then_some(&self.packet[..])
    }

    /// Drops a completed packet once it has been read.
    fn clear(&mut self) {
        self.packet.clear();
    }
}

/// Names every service the blocks of one DTVCC packet identify.
///
/// Walks the packet block by block, because a packet may carry several
/// back-to-back service blocks and only `block_size` says where the next
/// header begins. Service 0 is the null block — padding that keeps the caption
/// clock running — and names nothing.
fn observe_services(packet: &[u8], found: &mut CaptionObservation) {
    let mut cursor = 0_usize;
    while let Some(&header) = packet.get(cursor) {
        cursor += 1;
        let mut service = (header & 0xE0) >> 5;
        let block_size = usize::from(header & 0x1F);
        // A null block header of all zeros is the packet's padding tail;
        // nothing meaningful follows it.
        if header == 0 {
            break;
        }
        // Service 7 with a non-empty block escapes to extended numbering,
        // which is the only way services 7..=63 are expressible.
        if service == EXTENDED_SERVICE_ESCAPE && block_size != 0 {
            let Some(&extended) = packet.get(cursor) else {
                break;
            };
            cursor += 1;
            // Two bits of null padding above six bits of service number.
            service = extended & 0x3F;
        }
        // A block whose payload runs past the packet is not a block: the bytes
        // that would follow belong to no service, so the header is more likely
        // to be payload misread as one. Declaring from it would put a service
        // in the playlist that no decoder will find. Stricter than a player's
        // parser deliberately — a player recovers from a wrong guess on the
        // next packet, whereas a manifest declaration persists.
        if cursor.saturating_add(block_size) > packet.len() {
            break;
        }
        found.observe_service(service);
        cursor = cursor.saturating_add(block_size);
    }
}

/// How the NAL units of an access unit are framed.
///
/// FLV, RTMP, and MP4 deliver length-prefixed NALs described by an `avcC` or
/// `hvcC` record; MPEG-TS delivers Annex B start codes. Both reach this node,
/// so the framing is resolved once per track rather than guessed per packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Framing {
    /// Length-prefixed, with the prefix width taken from `avcC` / `hvcC`.
    LengthPrefixed {
        length_size: usize,
    },
    AnnexB,
}

/// Which video codec's NAL header and SEI types to walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SeiCodec {
    Avc,
    Hevc,
}

/// Scans one H.264 or HEVC track's access units for ATSC A53 caption data.
///
/// The type name is historical: HEVC uses the same A53 payload in prefix and
/// suffix SEI, so one detector covers both walks. AV1 is a different structure
/// (Metadata OBUs) and is not inspected here.
pub struct H264CaptionDetector {
    codec: SeiCodec,
    framing: Framing,
    observed: CaptionObservation,
    /// Access units seen, whether inspected or skipped by the sampling policy.
    seen: u64,
    /// Malformed SEI encountered, which is reported rather than fatal.
    malformed: u64,
    /// Carries a DTVCC packet across the access units it spans.
    ///
    /// Held by the detector rather than rebuilt per access unit, since a
    /// packet that straddles a boundary is only assemblable if the bytes from
    /// the first survive to meet the second.
    dtvcc: DtvccAssembler,
}

impl H264CaptionDetector {
    /// Every access unit is inspected while this many have been seen.
    ///
    /// Captions from a conformant publisher appear almost immediately, so a
    /// dense opening window answers the question quickly. At 30fps this is
    /// about ten seconds, which is also long enough to make "declared but never
    /// observed" a meaningful warning rather than a race.
    pub const VERIFICATION_WINDOW_ACCESS_UNITS: u64 = 300;

    /// One access unit in this many is inspected after the opening window.
    ///
    /// Scanning cannot stop outright: an arbitrary publisher may start sending
    /// captions minutes in, and a stream that went quiet is not the same as one
    /// that never carried them. Sampling keeps that discoverable at a cost that
    /// does not scale with bitrate.
    pub const SAMPLE_INTERVAL_ACCESS_UNITS: u64 = 30;

    /// Builds a detector for an H.264 or HEVC track, or `None` for anything else.
    ///
    /// The framing is derived from the track's codec configuration: a parsable
    /// `avcC` / `hvcC` record means length-prefixed NALs, and its absence means
    /// the track is carried as Annex B.
    pub fn new(track: &DiscoveredTrack) -> Option<Self> {
        let (codec, framing) = match track.codec {
            Codec::H264 => (SeiCodec::Avc, avc_framing(track.codec_extradata.as_bytes())),
            Codec::Hevc => (
                SeiCodec::Hevc,
                hevc_framing(track.codec_extradata.as_bytes()),
            ),
            _ => return None,
        };
        Some(Self {
            codec,
            framing,
            observed: CaptionObservation::default(),
            seen: 0,
            malformed: 0,
            dtvcc: DtvccAssembler::default(),
        })
    }

    /// Everything this track has been observed to carry.
    pub fn observed(&self) -> CaptionObservation {
        self.observed
    }

    /// Access units skipped or inspected so far.
    pub fn access_units_seen(&self) -> u64 {
        self.seen
    }

    /// Malformed SEI messages ignored so far.
    ///
    /// A publisher emitting these is still publishable — the captions are the
    /// only thing affected — so they are counted rather than raised.
    pub fn malformed_sei(&self) -> u64 {
        self.malformed
    }

    /// Whether this access unit should be inspected.
    ///
    /// Dense during the opening verification window, then periodic. The
    /// interval is measured *from the end of the window* rather than from the
    /// start of the stream, so the two constants can be tuned independently:
    /// counting from zero would silently skip a whole interval whenever the
    /// window is not an exact multiple of it.
    fn should_scan(&self) -> bool {
        let Some(since_window) = self
            .seen
            .checked_sub(Self::VERIFICATION_WINDOW_ACCESS_UNITS)
        else {
            return true;
        };
        since_window.is_multiple_of(Self::SAMPLE_INTERVAL_ACCESS_UNITS)
    }

    /// Inspects one access unit, returning what it added to the observation.
    ///
    /// Reports `true` when this access unit revealed a channel not previously
    /// seen, so a caller can republish a topology only on a real transition.
    pub fn inspect(&mut self, access_unit: &[u8]) -> bool {
        let scan = self.should_scan();
        self.seen = self.seen.saturating_add(1);
        if !scan {
            return false;
        }

        let before = self.observed;
        let mut found = CaptionObservation::default();
        let mut malformed = 0_u64;
        for nal in nal_units(access_unit, self.framing) {
            // Only SEI carries captions. Restricting the scan to this NAL type
            // is also what keeps compressed slice data from producing a false
            // positive when it happens to contain the A53 signature bytes.
            if !is_caption_sei(nal, self.codec) {
                continue;
            }
            match scan_sei(nal, self.codec, &mut self.dtvcc) {
                Ok(observation) => found.merge(observation),
                Err(()) => malformed = malformed.saturating_add(1),
            }
        }
        self.malformed = self.malformed.saturating_add(malformed);
        self.observed.merge(found);
        self.observed != before
    }
}

/// Reads the NAL length width from an `avcC` record, falling back to Annex B.
fn avc_framing(extradata: &[u8]) -> Framing {
    AvcDecoderConfigurationRecord::try_from(extradata).map_or(Framing::AnnexB, |record| {
        Framing::LengthPrefixed {
            length_size: usize::from(record.length_size_minus_one()) + 1,
        }
    })
}

/// Reads the NAL length width from an `hvcC` record, falling back to Annex B.
fn hevc_framing(extradata: &[u8]) -> Framing {
    transmux::HEVCDecoderConfigurationRecord::parse(extradata).map_or(Framing::AnnexB, |record| {
        Framing::LengthPrefixed {
            length_size: usize::from(record.length_size_minus_one) + 1,
        }
    })
}

/// Yields each NAL unit of an access unit, without its framing.
fn nal_units(access_unit: &[u8], framing: Framing) -> Vec<&[u8]> {
    match framing {
        Framing::LengthPrefixed { length_size } => {
            length_prefixed_nal_units(access_unit, length_size)
        }
        Framing::AnnexB => annex_b_nal_units(access_unit),
    }
}

fn length_prefixed_nal_units(access_unit: &[u8], length_size: usize) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut cursor = 0_usize;
    while cursor + length_size <= access_unit.len() {
        let mut length = 0_usize;
        for offset in 0..length_size {
            length = (length << 8) | usize::from(access_unit[cursor + offset]);
        }
        cursor += length_size;
        // A length running past the end means the framing assumption is wrong
        // for this packet; stopping keeps a misread from being reported as
        // caption data.
        let Some(nal) = access_unit.get(cursor..cursor.saturating_add(length)) else {
            break;
        };
        cursor = cursor.saturating_add(length);
        units.push(nal);
    }
    units
}

fn annex_b_nal_units(access_unit: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0_usize;
    while let Some(found) = find_start_code(access_unit, index) {
        starts.push(found);
        // Resume *at* the byte after the delimiter rather than past it. An
        // access unit may begin with two start codes back to back, and skipping
        // ahead would fold the following NAL into an empty one whose header
        // byte is really a prefix byte — which then fails the SEI check and
        // loses the captions. Emulation prevention guarantees a conformant NAL
        // body contains no start code, so this cannot loop.
        index = found;
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(position, &start)| {
            let end = starts.get(position + 1).map_or(access_unit.len(), |&next| {
                trim_trailing_zero(access_unit, next)
            });
            access_unit.get(start..end)
        })
        .collect()
}

/// Finds the byte after the next `00 00 01` start code prefix.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|window| window == [0x00, 0x00, 0x01])
        .map(|offset| from + offset + 3)
}

/// Drops the zero byte belonging to a four-byte start code prefix.
fn trim_trailing_zero(data: &[u8], next_start: usize) -> usize {
    let end = next_start.saturating_sub(3);
    if end > 0 && data.get(end - 1) == Some(&0x00) {
        end - 1
    } else {
        end
    }
}

/// Whether this NAL is an SEI unit that can carry A53 captions.
fn is_caption_sei(nal: &[u8], codec: SeiCodec) -> bool {
    match codec {
        SeiCodec::Avc => nal
            .first()
            .is_some_and(|header| header & 0x1F == NAL_UNIT_TYPE_SEI),
        SeiCodec::Hevc => {
            // HEVC NAL headers are two bytes: type in bits 1..=6 of the first,
            // layer id split across both. Captions live on the base layer.
            let [first, second, ..] = nal else {
                return false;
            };
            let nal_type = (first >> 1) & 0x3F;
            let layer_id = ((first & 0x01) << 5) | (second >> 3);
            layer_id == 0 && matches!(nal_type, HEVC_NAL_PREFIX_SEI | HEVC_NAL_SUFFIX_SEI)
        }
    }
}

/// Reads every SEI message in one NAL, reporting the caption data it carries.
///
/// `Err` means the SEI structure could not be read, which is a publisher
/// problem rather than a session-ending one.
fn scan_sei(
    nal: &[u8],
    codec: SeiCodec,
    dtvcc: &mut DtvccAssembler,
) -> Result<CaptionObservation, ()> {
    // HEVC's NAL header is two bytes. `decode_nal` always strips one, so the
    // extra byte is skipped first and the H.264 RBSP decoder handles the rest.
    let to_decode = match codec {
        SeiCodec::Avc => nal,
        SeiCodec::Hevc => nal.get(1..).ok_or(())?,
    };
    let rbsp = rbsp::decode_nal(to_decode).map_err(|_| ())?;
    let mut scratch = Vec::new();
    let mut reader = SeiReader::from_rbsp_bytes(&rbsp[..], &mut scratch);
    let mut found = CaptionObservation::default();
    loop {
        match reader.next() {
            Ok(Some(message)) => {
                if message.payload_type != HeaderType::UserDataRegisteredItuTT35 {
                    continue;
                }
                let Ok((country, rest)) = ItuTT35::read(&message) else {
                    continue;
                };
                if let Some(observation) = recognize_a53(&country, rest, dtvcc) {
                    found.merge(observation);
                }
            }
            Ok(None) => return Ok(found),
            Err(_) => return Err(()),
        }
    }
}

/// Recognizes the ATSC A53 signature and classifies the channels it carries.
///
/// `rest` begins at the provider code: `ItuTT35::read` has already consumed the
/// country code.
fn recognize_a53(
    country: &ItuTT35,
    rest: &[u8],
    dtvcc: &mut DtvccAssembler,
) -> Option<CaptionObservation> {
    if *country != ItuTT35::UnitedStates
        || rest.get(..2)? != ATSC_PROVIDER_CODE
        || rest.get(2..6)? != ATSC_USER_IDENTIFIER
        || *rest.get(6)? != USER_DATA_TYPE_CC_DATA
    {
        return None;
    }
    let flags = *rest.get(7)?;
    // Without process_cc_data_flag the triplets carry nothing a player would
    // act on, so this is not evidence of captions.
    if flags & 0x40 == 0 {
        return None;
    }
    let cc_count = usize::from(flags & 0x1F);
    let triplets = rest.get(CC_DATA_HEADER_LEN..CC_DATA_HEADER_LEN + cc_count * 3)?;

    let mut found = CaptionObservation::default();
    for triplet in triplets.as_chunks::<3>().0 {
        // cc_valid distinguishes a carried pair from padding that only keeps
        // the caption clock running.
        if triplet[0] & 0x04 == 0 {
            continue;
        }
        found.a53_present = true;
        match triplet[0] & 0x03 {
            // CEA-608 Line 21 fields. Field 1 carries CC1/CC2 and field 2
            // carries CC3/CC4; which of the two channels is in use cannot be
            // told apart without decoding the control codes.
            field @ (0 | 1) => found.cea608_fields |= 1 << field,
            // DTVCC data. cc_type 3 announces a packet and carries its header
            // plus the first payload byte; cc_type 2 continues one. Both bytes
            // of the triplet belong to the packet.
            cc_type => {
                found.dtvcc_present = true;
                if cc_type == 3 {
                    dtvcc.begin(triplet[1]);
                    dtvcc.push(triplet[2]);
                } else {
                    dtvcc.push(triplet[1]);
                    dtvcc.push(triplet[2]);
                }
                if let Some(packet) = dtvcc.complete() {
                    observe_services(packet, &mut found);
                    dtvcc.clear();
                }
            }
        }
    }
    found.a53_present.then_some(found)
}

#[cfg(test)]
mod tests {
    use crate::domain::{MediaParameters, Payload, Timebase, TrackId};

    use super::*;

    /// CEA-608 bytes carry odd parity in the high bit.
    fn parity(byte: u8) -> u8 {
        let value = byte & 0x7F;
        if value.count_ones() % 2 == 1 {
            value
        } else {
            value | 0x80
        }
    }

    /// Builds a `cc_data` payload carrying `(cc_type, first, second)` triplets.
    fn cc_data(triplets: &[(u8, u8, u8)]) -> Vec<u8> {
        let mut payload = vec![0xB5];
        payload.extend_from_slice(&ATSC_PROVIDER_CODE);
        payload.extend_from_slice(ATSC_USER_IDENTIFIER);
        payload.push(USER_DATA_TYPE_CC_DATA);
        // process_em_data_flag, process_cc_data_flag, additional_data_flag=0,
        // then five bits of cc_count.
        payload.push(0xC0 | u8::try_from(triplets.len()).expect("fixture triplet count fits"));
        payload.push(0xFF);
        for &(cc_type, first, second) in triplets {
            payload.push(0xF8 | 0x04 | (cc_type & 0x03));
            if cc_type < 2 {
                payload.push(parity(first));
                payload.push(parity(second));
            } else {
                // DTVCC bytes carry no parity.
                payload.push(first);
                payload.push(second);
            }
        }
        payload.push(0xFF);
        payload
    }

    /// Inserts emulation-prevention bytes so a payload cannot imitate a start
    /// code.
    fn emulation_prevent(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut zeros = 0;
        for &byte in data {
            if zeros >= 2 && byte <= 0x03 {
                out.push(0x03);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    /// Wraps a payload as an SEI NAL of the given payload type.
    fn sei_nal(payload_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![payload_type];
        let mut remaining = payload.len();
        while remaining >= 255 {
            body.push(0xFF);
            remaining -= 255;
        }
        body.push(u8::try_from(remaining).expect("fixture payload size fits"));
        body.extend_from_slice(payload);
        body.push(0x80); // rbsp_trailing_bits

        let mut nal = vec![0x06]; // nal_ref_idc=0, nal_unit_type=6 (SEI)
        nal.extend_from_slice(&emulation_prevent(&body));
        nal
    }

    /// Frames NAL units as a length-prefixed AVCC access unit.
    fn avcc_access_unit(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            let length = u32::try_from(nal.len()).expect("fixture NAL length fits");
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    /// Frames NAL units as an Annex B access unit.
    fn annex_b_access_unit(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// A minimal coded slice, which must never be inspected for captions.
    fn slice_nal(payload: &[u8]) -> Vec<u8> {
        let mut nal = vec![0x65]; // nal_ref_idc=3, nal_unit_type=5 (IDR)
        nal.extend_from_slice(payload);
        nal
    }

    /// Builds one service block: header, optional extended number, payload.
    fn service_block(service: u8, payload: &[u8]) -> Vec<u8> {
        let size = u8::try_from(payload.len()).expect("fixture block size fits");
        assert!(size <= 0x1F, "a service block carries at most 31 bytes");
        let mut block = Vec::new();
        if service <= 6 {
            block.push((service << 5) | size);
        } else {
            // Services 7..=63 are only expressible through the escape: a
            // header naming service 7, then the real number in six bits.
            block.push((EXTENDED_SERVICE_ESCAPE << 5) | size);
            block.push(service & 0x3F);
        }
        block.extend_from_slice(payload);
        block
    }

    /// Wraps service blocks as a DTVCC packet, then as `cc_data` triplets.
    ///
    /// The packet header's size code counts byte *pairs* including itself, so
    /// the packet is padded to an odd number of following bytes — which is how
    /// a real encoder emits it, and what makes the round trip meaningful.
    fn dtvcc_triplets(blocks: &[Vec<u8>]) -> Vec<(u8, u8, u8)> {
        let mut body: Vec<u8> = blocks.concat();
        // packet_size_code * 2 == total packet bytes, header included.
        let mut size_code = (body.len() + 1).div_ceil(2);
        if size_code * 2 < body.len() + 1 {
            size_code += 1;
        }
        body.resize(size_code * 2 - 1, 0x00);

        let mut triplets = Vec::new();
        let header = u8::try_from(size_code).expect("fixture size code fits");
        let mut bytes = body.into_iter();
        // cc_type 3 carries the packet header plus the first payload byte.
        triplets.push((3, header, bytes.next().unwrap_or(0x00)));
        while let Some(first) = bytes.next() {
            triplets.push((2, first, bytes.next().unwrap_or(0x00)));
        }
        triplets
    }

    /// An access unit whose SEI carries the given DTVCC service blocks.
    fn dtvcc_access_unit(blocks: &[Vec<u8>]) -> Vec<u8> {
        let triplets = dtvcc_triplets(blocks);
        avcc_access_unit(&[sei_nal(4, &cc_data(&triplets)), slice_nal(&[0x88; 16])])
    }

    /// An `avcC` record declaring four-byte NAL length prefixes.
    fn avcc_extradata() -> Vec<u8> {
        vec![
            0x01, // configurationVersion
            0x64, 0x00, 0x1E, // profile, compatibility, level
            0xFF, // 6 reserved bits, lengthSizeMinusOne=3
            0xE1, // 3 reserved bits, numOfSequenceParameterSets=1
            0x00, 0x04, 0x67, 0x64, 0x00, 0x1E, // one SPS
            0x01, // numOfPictureParameterSets
            0x00, 0x02, 0x68, 0xEE, // one PPS
        ]
    }

    /// A minimal `hvcC` record declaring four-byte NAL length prefixes.
    fn hvcc_extradata() -> Vec<u8> {
        let mut record = vec![0_u8; 23];
        record[0] = 1;
        record[13] = 0xF0;
        record[15] = 0xFC;
        record[16] = 0xFC;
        record[17] = 0xF8;
        record[18] = 0xF8;
        record[21] = 0x03; // lengthSizeMinusOne = 3
        record
    }

    /// Wraps a payload as an HEVC prefix or suffix SEI NAL.
    fn hevc_sei_nal(nal_type: u8, payload_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![payload_type];
        let mut remaining = payload.len();
        while remaining >= 255 {
            body.push(0xFF);
            remaining -= 255;
        }
        body.push(u8::try_from(remaining).expect("fixture payload size fits"));
        body.extend_from_slice(payload);
        body.push(0x80);

        let mut nal = vec![nal_type << 1, 0x01];
        nal.extend_from_slice(&emulation_prevent(&body));
        nal
    }

    /// A minimal HEVC coded slice, which must never be inspected for captions.
    fn hevc_slice_nal(payload: &[u8]) -> Vec<u8> {
        let mut nal = vec![19 << 1, 0x01]; // IDR_W_RADL, layer 0
        nal.extend_from_slice(payload);
        nal
    }

    fn track(codec: Codec, extradata: Vec<u8>) -> DiscoveredTrack {
        DiscoveredTrack {
            decoder_config_origin: crate::domain::DecoderConfigOrigin::Publisher,
            video_cadence: crate::domain::VideoCadence::Unknown,
            id: TrackId(0),
            source_key: None,
            codec,
            parameters: MediaParameters::Video {
                width: nz::u32!(1920),
                height: nz::u32!(1080),
                frame_rate: None,
                video_delay: 0,
            },
            timebase: Timebase::new(nz::u32!(1), nz::u32!(90_000)),
            first_pts: None,
            title: None,
            language: None,
            codec_extradata: Payload::from(extradata),
        }
    }

    #[test]
    fn cea608_field_one_is_recognized_in_an_avcc_access_unit() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // EDM, then two displayable characters, all on field 1.
        let captions = sei_nal(
            4,
            &cc_data(&[(0, 0x14, 0x2C), (0, 0x94, 0x20), (0, 0x48, 0x49)]),
        );
        let access_unit = avcc_access_unit(&[captions, slice_nal(&[0x88; 32])]);

        assert!(detector.inspect(&access_unit));
        let observed = detector.observed();
        assert!(observed.a53_present);
        assert_eq!(observed.cea608_fields, 0b01);
        assert!(!observed.dtvcc_present);
    }

    #[test]
    fn a_dtvcc_only_stream_reports_no_cea608_fields() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // What a transcoder emits with 608 carriage disabled: a DTVCC packet
        // start and continuation, and no Line 21 triplets at all. Classifying
        // either of these as CEA-608 would put the wrong INSTREAM-ID in a
        // playlist, so this is pinned separately from the 608 case.
        let captions = sei_nal(4, &cc_data(&[(3, 0x02, 0x21), (2, 0x41, 0x42)]));
        let access_unit = avcc_access_unit(&[captions, slice_nal(&[0x88; 32])]);

        assert!(detector.inspect(&access_unit));
        let observed = detector.observed();
        assert!(observed.a53_present);
        assert_eq!(observed.cea608_fields, 0);
        assert!(observed.dtvcc_present);
    }

    #[test]
    fn annex_b_framing_is_scanned_without_an_avcc_record() {
        // MPEG-TS ingest carries no avcC record, so the detector must fall back
        // to start-code framing rather than misreading the first bytes as a
        // NAL length.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, Vec::new())).expect("h264 track");
        let captions = sei_nal(4, &cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = annex_b_access_unit(&[captions, slice_nal(&[0x88; 32])]);

        assert!(detector.inspect(&access_unit));
        assert!(detector.observed().a53_present);
        assert_eq!(detector.observed().cea608_fields, 0b01);
    }

    #[test]
    fn an_unrelated_sei_message_is_not_mistaken_for_captions() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // Payload type 5 is user_data_unregistered, which every x264 encoder
        // emits to record its build. It is the false positive that matters,
        // because it is an SEI message present in essentially every stream.
        let version = sei_nal(5, b"x264 - core 165 r3222");
        let access_unit = avcc_access_unit(&[version, slice_nal(&[0x88; 32])]);

        assert!(!detector.inspect(&access_unit));
        assert_eq!(detector.observed(), CaptionObservation::default());
    }

    #[test]
    fn caption_bytes_inside_slice_data_are_not_inspected() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // Compressed slice data can contain any byte sequence, including one
        // that looks like an A53 signature. Restricting the scan to SEI NALs is
        // what makes that harmless.
        let mut disguised = vec![0x00, 0x00, 0x00];
        disguised.extend_from_slice(&cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = avcc_access_unit(&[slice_nal(&disguised)]);

        assert!(!detector.inspect(&access_unit));
        assert!(!detector.observed().a53_present);
    }

    #[test]
    fn padding_without_valid_triplets_is_not_evidence_of_captions() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // cc_valid clear: the caption clock is running but nothing is carried.
        let mut payload = vec![0xB5];
        payload.extend_from_slice(&ATSC_PROVIDER_CODE);
        payload.extend_from_slice(ATSC_USER_IDENTIFIER);
        payload.push(USER_DATA_TYPE_CC_DATA);
        payload.push(0xC0 | 1);
        payload.push(0xFF);
        payload.extend_from_slice(&[0xF8, 0x80, 0x80]);
        payload.push(0xFF);
        let access_unit = avcc_access_unit(&[sei_nal(4, &payload), slice_nal(&[0x88; 16])]);

        assert!(!detector.inspect(&access_unit));
        assert!(!detector.observed().a53_present);
    }

    #[test]
    fn a_truncated_sei_is_counted_rather_than_raised() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // A payload size larger than the bytes that follow. A publisher sending
        // this is still publishable; only its captions are affected.
        let malformed = vec![0x06, 0x04, 0x40, 0xB5, 0x00];
        let access_unit = avcc_access_unit(&[malformed, slice_nal(&[0x88; 16])]);

        assert!(!detector.inspect(&access_unit));
        assert_eq!(detector.malformed_sei(), 1);
        assert!(!detector.observed().a53_present);
    }

    #[test]
    fn only_a_new_channel_reports_a_transition() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        let captions = sei_nal(4, &cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = avcc_access_unit(&[captions, slice_nal(&[0x88; 16])]);

        // The first observation is a transition; repeating it is not, which is
        // what keeps a per-packet scan from republishing the topology forever.
        assert!(detector.inspect(&access_unit));
        assert!(!detector.inspect(&access_unit));
        assert_eq!(detector.access_units_seen(), 2);
    }

    #[test]
    fn an_av1_track_has_no_detector() {
        assert!(H264CaptionDetector::new(&track(Codec::Av1, Vec::new())).is_none());
    }

    #[test]
    fn cea608_is_recognized_in_an_hevc_prefix_sei() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::Hevc, hvcc_extradata())).expect("hevc track");
        let captions = hevc_sei_nal(HEVC_NAL_PREFIX_SEI, 4, &cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = avcc_access_unit(&[captions, hevc_slice_nal(&[0x88; 32])]);

        assert!(detector.inspect(&access_unit));
        assert!(detector.observed().a53_present);
        assert_eq!(detector.observed().cea608_fields, 0b01);
    }

    #[test]
    fn cea608_is_recognized_in_an_hevc_suffix_sei() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::Hevc, hvcc_extradata())).expect("hevc track");
        let captions = hevc_sei_nal(HEVC_NAL_SUFFIX_SEI, 4, &cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = avcc_access_unit(&[hevc_slice_nal(&[0x88; 16]), captions]);

        assert!(detector.inspect(&access_unit));
        assert_eq!(detector.observed().cea608_fields, 0b01);
    }

    #[test]
    fn hevc_annex_b_framing_is_scanned_without_an_hvcc_record() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::Hevc, Vec::new())).expect("hevc track");
        let captions = hevc_sei_nal(HEVC_NAL_PREFIX_SEI, 4, &cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = annex_b_access_unit(&[captions, hevc_slice_nal(&[0x88; 32])]);

        assert!(detector.inspect(&access_unit));
        assert_eq!(detector.observed().cea608_fields, 0b01);
    }

    #[test]
    fn hevc_caption_bytes_inside_slice_data_are_not_inspected() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::Hevc, hvcc_extradata())).expect("hevc track");
        let mut disguised = vec![0x00, 0x00, 0x00];
        disguised.extend_from_slice(&cc_data(&[(0, 0x14, 0x2C)]));
        let access_unit = avcc_access_unit(&[hevc_slice_nal(&disguised)]);

        assert!(!detector.inspect(&access_unit));
        assert!(!detector.observed().a53_present);
    }

    #[test]
    fn hevc_sei_on_a_non_base_layer_is_ignored() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::Hevc, hvcc_extradata())).expect("hevc track");
        let mut captions = hevc_sei_nal(HEVC_NAL_PREFIX_SEI, 4, &cc_data(&[(0, 0x14, 0x2C)]));
        captions[0] |= 0x01; // nuh_layer_id high bit: captions live on layer 0
        let access_unit = avcc_access_unit(&[captions, hevc_slice_nal(&[0x88; 32])]);

        assert!(!detector.inspect(&access_unit));
        assert!(!detector.observed().a53_present);
    }

    /// The services a detector named, lowest first.
    fn services_of(observed: CaptionObservation) -> Vec<u8> {
        (1..=MAXIMUM_DTVCC_SERVICE)
            .filter(|service| observed.cea708_services & (1 << u32::from(service - 1)) != 0)
            .collect()
    }

    #[test]
    fn a_dtvcc_service_is_named_rather_than_assumed_to_be_the_first() {
        // The bug this replaces: every 708 stream was declared SERVICE1, so a
        // publisher using service 4 got an INSTREAM-ID naming a service no
        // decoder would find.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        assert!(detector.inspect(&dtvcc_access_unit(&[service_block(4, &[0x41])])));

        assert_eq!(services_of(detector.observed()), vec![4]);
        assert!(detector.observed().dtvcc_present);
    }

    #[test]
    fn every_service_block_in_a_packet_is_named() {
        // Blocks sit back to back, and only block_size says where the next
        // header begins. Reading just the first would miss service 2 entirely.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        assert!(detector.inspect(&dtvcc_access_unit(&[
            service_block(1, &[0x20, 0x67]),
            service_block(2, &[0x41]),
        ])));

        assert_eq!(services_of(detector.observed()), vec![1, 2]);
    }

    #[test]
    fn an_extended_service_number_is_read_from_the_escape_header() {
        // Services above 6 exist only through the escape, and SERVICE42 is one
        // of the spec's own examples, so a stream using it must not be
        // silently declared as service 7.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        assert!(detector.inspect(&dtvcc_access_unit(&[service_block(42, &[0x41, 0x42])])));

        assert_eq!(services_of(detector.observed()), vec![42]);
    }

    #[test]
    fn null_padding_alone_names_no_service() {
        // Service 0 is the null block: padding that keeps the caption clock
        // running. Declaring a caption track for it would promise a service
        // that carries no text, which is the false positive that makes
        // dtvcc_present unusable as a declaration on its own.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        detector.inspect(&dtvcc_access_unit(&[service_block(0, &[])]));

        assert!(detector.observed().dtvcc_present);
        assert_eq!(detector.observed().cea708_services, 0);
        assert!(services_of(detector.observed()).is_empty());
    }

    #[test]
    fn a_packet_spanning_two_access_units_is_still_named() {
        // A packet normally fits one cc_data payload, but nothing guarantees
        // it. The carry-over buffer is what keeps a split packet from being
        // abandoned — and from having its second half misread as a header.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        let triplets = dtvcc_triplets(&[service_block(3, &[0x41, 0x42, 0x43, 0x44])]);
        let split = triplets.len() / 2;
        assert!(split > 0, "the fixture packet spans several triplets");

        let first = avcc_access_unit(&[
            sei_nal(4, &cc_data(&triplets[..split])),
            slice_nal(&[0x88; 16]),
        ]);
        let second = avcc_access_unit(&[
            sei_nal(4, &cc_data(&triplets[split..])),
            slice_nal(&[0x88; 16]),
        ]);

        // The first half completes no packet, so it names nothing.
        detector.inspect(&first);
        assert_eq!(detector.observed().cea708_services, 0);

        detector.inspect(&second);
        assert_eq!(services_of(detector.observed()), vec![3]);
    }

    #[test]
    fn the_captured_stream_packet_names_service_one() {
        // Bytes lifted from a real capture rather than built here: header 0x45
        // promises nine following bytes, block header 0x27 names service 1
        // with a seven-byte block, and the trailing 0x00 is null padding. A
        // five-bit reading of 0x27 would report service 4, so this pins the
        // three-bit split against a stream that actually exists.
        let mut found = CaptionObservation::default();
        observe_services(
            &[0x27, 0x20, 0x67, 0x72, 0x65, 0x61, 0x74, 0x03, 0x00],
            &mut found,
        );

        assert_eq!(services_of(found), vec![1]);
    }

    #[test]
    fn a_block_running_past_the_packet_names_nothing() {
        // A header whose payload does not fit is more likely payload misread
        // as a header than a real block. A player recovers on the next packet;
        // a manifest declaration persists, so this is refused outright.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        // Service 5 claiming 31 bytes inside a packet that carries two.
        let overrun = vec![(5_u8 << 5) | 0x1F, 0x41];
        let triplets = dtvcc_triplets(&[overrun]);
        detector.inspect(&avcc_access_unit(&[
            sei_nal(4, &cc_data(&triplets)),
            slice_nal(&[0x88; 16]),
        ]));

        assert_eq!(detector.observed().cea708_services, 0);
    }

    #[test]
    fn consecutive_annex_b_start_codes_do_not_hide_the_next_nal() {
        // Encoders emit back-to-back start codes at access-unit boundaries. A
        // scanner that resumes searching *past* the delimiter it just found
        // swallows the following NAL into one whose header byte is the second
        // prefix's, which then fails the SEI filter and loses the captions.
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, Vec::new())).expect("h264 track");
        let captions = sei_nal(4, &cc_data(&[(0, 0x14, 0x2C)]));

        let mut access_unit = Vec::new();
        access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        access_unit.extend_from_slice(&captions);
        access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        access_unit.extend_from_slice(&slice_nal(&[0x88; 16]));

        assert!(detector.inspect(&access_unit));
        assert!(detector.observed().a53_present);
    }

    #[test]
    fn sampling_continues_at_a_fixed_rate_after_the_verification_window() {
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        let plain = avcc_access_unit(&[slice_nal(&[0x88; 8])]);
        // Exhaust the dense window so only the sampling rule applies.
        for _ in 0..H264CaptionDetector::VERIFICATION_WINDOW_ACCESS_UNITS {
            detector.inspect(&plain);
        }

        // Captions that begin after the window must still be found within one
        // sampling interval, whatever the two constants are tuned to.
        let captions = avcc_access_unit(&[
            sei_nal(4, &cc_data(&[(0, 0x14, 0x2C)])),
            slice_nal(&[0x88; 8]),
        ]);
        let mut detected = false;
        for _ in 0..H264CaptionDetector::SAMPLE_INTERVAL_ACCESS_UNITS {
            detected |= detector.inspect(&captions);
        }
        assert!(detected, "an interval must contain a scanned access unit");
    }

    #[test]
    fn hostile_sei_never_panics() {
        // `h264-reader` is the only third-party parser on the packet path, and
        // it is fed bytes a publisher controls. Nothing it reports may escalate
        // past the malformed counter, and nothing may panic — a caption scan
        // must never be able to take a session down.
        let mut state = 0x5eed_1234_abcd_0001_u64;
        let mut detector =
            H264CaptionDetector::new(&track(Codec::H264, avcc_extradata())).expect("h264 track");
        let mut annex_b =
            H264CaptionDetector::new(&track(Codec::H264, Vec::new())).expect("h264 track");
        let mut hevc =
            H264CaptionDetector::new(&track(Codec::Hevc, hvcc_extradata())).expect("hevc track");

        for _ in 0..2_000 {
            let length =
                usize::try_from(crate::test_fuzz::next_random(&mut state) % 96).unwrap_or(0);
            let mut unit: Vec<u8> = (0..length)
                .map(|_| u8::try_from(crate::test_fuzz::next_random(&mut state) % 256).unwrap_or(0))
                .collect();
            // Bias towards structures that reach deepest: a plausible SEI
            // header and a start-code prefix, so the fuzz spends its budget
            // inside the parser rather than being rejected at the first byte.
            if !unit.is_empty() && unit[0].is_multiple_of(2) {
                unit.splice(0..0, [0x00, 0x00, 0x00, 0x01, 0x06, 0x04]);
            }
            detector.inspect(&unit);
            annex_b.inspect(&unit);
            hevc.inspect(&unit);
        }
    }
}

//! Reconciles observed in-band captions across every video track.
//!
//! HLS declares closed captions once for a presentation, and a client assumes
//! the declared services are present in *every* video rendition it might switch
//! to. A ladder that carries captions on one rendition but not another is
//! therefore not a partial success: switching mid-playback would silently drop
//! the captions, which is exactly the inconsistency
//! draft-pantos-hls-rfc8216bis-22 section 4.4.6.2 warns about.
//!
//! So this tracks each video track separately and only declares a service once
//! every video track has been seen carrying it.

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    domain::{DiscoveredTrack, MediaKind, TrackId},
    mux::{CaptionChannel, ClosedCaptionService},
    source::{CaptionObservation, H264CaptionDetector},
};

/// Highest CEA-708 service number a service block header can name.
///
/// CEA-708-E permits 1..=63, which is also the range
/// draft-pantos-hls-rfc8216bis-22 section 4.4.6.1 allows in an `INSTREAM-ID`.
const MAXIMUM_DTVCC_SERVICE: u8 = 63;

/// How a presentation's caption declaration compares to what was observed.
///
/// Reported rather than acted on: every state except [`Self::Consistent`] is an
/// operator-facing fact about a publisher, not something this node can fix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptionReconciliation {
    /// No video track has been observed carrying captions.
    Absent,
    /// Every video track carries the same services.
    Consistent,
    /// Some, but not all, video tracks carry captions.
    ///
    /// The dangerous case: declaring would promise captions that vanish when a
    /// client switches renditions, so nothing is declared while this holds.
    PartialLadder,
    /// Every video track carries captions, but not the same channels.
    ///
    /// Distinct from [`Self::PartialLadder`] because the ladder is not partly
    /// uncaptioned — it is inconsistently captioned, which needs a different
    /// fix from the operator. Nothing is declared either way: a channel absent
    /// from one rendition cannot be promised for the presentation.
    ChannelMismatch,
}

/// Watches every video track of a publication for in-band caption data.
pub struct CaptionVerifier {
    detectors: BTreeMap<TrackId, H264CaptionDetector>,
    /// Video tracks that carry no detector, because nothing can inspect them.
    ///
    /// A presentation containing one cannot honestly declare captions: an
    /// undetectable rendition is indistinguishable from one that carries
    /// nothing.
    unverifiable: usize,
    language: Option<Arc<str>>,
    declared: Arc<[ClosedCaptionService]>,
}

impl CaptionVerifier {
    /// Builds a verifier over the video tracks of a publication.
    ///
    /// `language` is what any declared service will advertise. HLS makes
    /// `LANGUAGE` optional on `EXT-X-MEDIA`, so its absence is not a reason to
    /// withhold a declaration — an undeclared caption reaches nobody, while a
    /// declared one without a language still reaches everybody.
    pub fn new<'a>(
        tracks: impl IntoIterator<Item = &'a DiscoveredTrack>,
        language: Option<Arc<str>>,
    ) -> Self {
        let mut detectors = BTreeMap::new();
        let mut unverifiable = 0;
        for track in tracks {
            if track.kind() != MediaKind::Video {
                continue;
            }
            match H264CaptionDetector::new(track) {
                Some(detector) => {
                    detectors.insert(track.id, detector);
                }
                None => unverifiable += 1,
            }
        }
        Self {
            detectors,
            unverifiable,
            language,
            declared: Arc::from([]),
        }
    }

    /// Inspects one access unit belonging to `track_id`.
    ///
    /// Returns the services to declare when this observation changed them, and
    /// `None` when nothing changed — which is the common case, so a caller can
    /// treat a return value as the rare event it is.
    pub fn inspect(
        &mut self,
        track_id: TrackId,
        access_unit: &[u8],
    ) -> Option<Arc<[ClosedCaptionService]>> {
        let detector = self.detectors.get_mut(&track_id)?;
        if !detector.inspect(access_unit) {
            return None;
        }
        let services = self.services();
        if services == self.declared {
            return None;
        }
        self.declared = Arc::clone(&services);
        Some(services)
    }

    /// How the observation across every video track currently reconciles.
    pub fn reconciliation(&self) -> CaptionReconciliation {
        let carrying = self
            .detectors
            .values()
            .filter(|detector| detector.observed().a53_present)
            .count();
        if carrying == 0 {
            return CaptionReconciliation::Absent;
        }
        if carrying != self.detectors.len() || self.unverifiable > 0 {
            return CaptionReconciliation::PartialLadder;
        }
        // Every track carries *something*, but agreeing on carriage is not the
        // same as agreeing on channels: a ladder offering CC1 on one rendition
        // and SERVICE1 on another has nothing in common to declare.
        match self.common_channels() {
            Some(_) => CaptionReconciliation::Consistent,
            None => CaptionReconciliation::ChannelMismatch,
        }
    }

    /// How many video tracks are known, and how many carry captions.
    ///
    /// Reported together because the interesting fact is the ratio: one of
    /// three renditions carrying captions is a broken ladder, while three of
    /// three is a healthy one.
    pub fn carriage(&self) -> (usize, usize) {
        let carrying = self
            .detectors
            .values()
            .filter(|detector| detector.observed().a53_present)
            .count();
        (
            carrying,
            self.detectors.len().saturating_add(self.unverifiable),
        )
    }

    /// Malformed SEI messages ignored across every video track.
    ///
    /// Surfaced so an operator can tell "this stream carries no captions" from
    /// "this stream's captions could not be read", which look identical from
    /// the playlist alone.
    pub fn malformed_sei(&self) -> u64 {
        self.detectors
            .values()
            .map(H264CaptionDetector::malformed_sei)
            .sum()
    }

    /// The channels currently declared to the presentation.
    pub fn declared_channels(&self) -> Vec<CaptionChannel> {
        self.declared
            .iter()
            .map(|service| service.channel)
            .collect()
    }

    /// The services every video track carries, as playlist declarations.
    ///
    /// Intersected rather than unioned: a service present on only part of the
    /// ladder is not something the presentation can promise.
    fn services(&self) -> Arc<[ClosedCaptionService]> {
        if self.reconciliation() != CaptionReconciliation::Consistent {
            return Arc::from([]);
        }
        let Some(common) = self.common_channels() else {
            return Arc::from([]);
        };
        common
            .into_iter()
            .enumerate()
            .map(|(position, channel)| ClosedCaptionService {
                channel,
                name: Arc::from(caption_name(channel)),
                language: self.language.clone(),
                // At most one member of a group may be DEFAULT=YES, so the
                // first channel is the primary and any 608 compatibility
                // service alongside it is merely selectable. Ordering comes
                // from `CaptionChannel`, which sorts 608 fields before 708
                // services.
                is_default: position == 0,
                // Every declared service is a legitimate automatic choice; a
                // client that asked for captions should be able to land on any
                // of them.
                autoselect: true,
            })
            .collect::<Vec<_>>()
            .into()
    }

    /// Channels observed on every video track, or `None` if there are none.
    fn common_channels(&self) -> Option<Vec<CaptionChannel>> {
        let mut common: Option<Vec<CaptionChannel>> = None;
        for detector in self.detectors.values() {
            let channels = channels_of(detector.observed());
            common = Some(match common {
                None => channels,
                Some(previous) => previous
                    .into_iter()
                    .filter(|channel| channels.contains(channel))
                    .collect(),
            });
        }
        common.filter(|channels| !channels.is_empty())
    }
}

/// The channels one observation names.
///
/// Both mappings are inferences, not identifications, and each can name a
/// channel the stream does not actually carry:
///
/// - A Line 21 field carries two channels (field 1 is CC1/CC2, field 2 is
///   CC3/CC4) and telling them apart requires decoding the control codes that
///   switch between them. The first channel of the field is assumed.
/// - A DTVCC packet's service number lives in a packet that may span several
///   access units, so carriage is taken as evidence of the primary service.
///
/// Both assumptions hold for conformant encoders, which use CC1 and SERVICE1
/// as the primary channels. A publisher that uses only CC2 or SERVICE2 as its
/// primary would be declared under the wrong `INSTREAM-ID`, and the player
/// would surface an empty caption track. Identifying them properly means
/// decoding 608 control codes and reassembling DTVCC packets — the stateful
/// work this detector exists to avoid.
fn channels_of(observed: CaptionObservation) -> Vec<CaptionChannel> {
    let mut channels = Vec::new();
    for field in 0..2 {
        if observed.cea608_fields & (1 << field) != 0 {
            channels.push(CaptionChannel::Cea608Field(field));
        }
    }
    // Only services a block header actually named. `dtvcc_present` is
    // deliberately not a fallback: a stream carrying nothing but null padding
    // sets it while naming no service, and declaring a caption track for
    // padding would promise a service that carries no text.
    for service in 1..=MAXIMUM_DTVCC_SERVICE {
        if observed.cea708_services & (1 << u32::from(service - 1)) != 0 {
            channels.push(CaptionChannel::Cea708Service(service));
        }
    }
    channels
}

/// A human-readable name for a channel, used when nothing else supplies one.
///
/// `NAME` is required on `EXT-X-MEDIA`, and no in-band caption carries a title,
/// so one is derived from the channel itself.
fn caption_name(channel: CaptionChannel) -> String {
    match channel {
        CaptionChannel::Cea608Field(field) => format!("CC{}", field * 2 + 1),
        CaptionChannel::Cea708Service(service) => format!("Service {service}"),
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{MediaParameters, Payload, Timebase};

    use super::*;

    /// An `avcC` record declaring four-byte NAL length prefixes.
    fn avcc_extradata() -> Vec<u8> {
        vec![
            0x01, 0x64, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x04, 0x67, 0x64, 0x00, 0x1E, 0x01, 0x00,
            0x02, 0x68, 0xEE,
        ]
    }

    fn video_track_with(
        id: u32,
        codec: crate::domain::Codec,
        extradata: Vec<u8>,
    ) -> DiscoveredTrack {
        DiscoveredTrack {
            id: TrackId(id),
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

    fn video_track(id: u32) -> DiscoveredTrack {
        video_track_with(id, crate::domain::Codec::H264, avcc_extradata())
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

    /// An HEVC length-prefixed access unit carrying one CEA-608 field 1 SEI.
    fn hevc_captioned_access_unit() -> Vec<u8> {
        let payload: &[u8] = &[
            0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xC1, 0xFF, 0xFC, 0x94, 0x2C, 0xFF,
        ];
        let mut sei = vec![39 << 1, 0x01, 0x04];
        sei.push(u8::try_from(payload.len()).expect("fixture payload fits"));
        sei.extend_from_slice(payload);
        sei.push(0x80);

        let mut unit = Vec::new();
        unit.extend_from_slice(
            &u32::try_from(sei.len())
                .expect("fixture NAL fits")
                .to_be_bytes(),
        );
        unit.extend_from_slice(&sei);
        unit
    }

    /// An AVCC access unit carrying one CEA-608 field 1 caption SEI.
    fn captioned_access_unit() -> Vec<u8> {
        // Built by hand rather than shared with the detector's fixtures so this
        // test fails if the wire format it depends on changes.
        let payload: &[u8] = &[
            0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xC1, 0xFF, 0xFC, 0x94, 0x2C, 0xFF,
        ];
        let mut sei = vec![0x06, 0x04];
        sei.push(u8::try_from(payload.len()).expect("fixture payload fits"));
        sei.extend_from_slice(payload);
        sei.push(0x80);

        let mut unit = Vec::new();
        unit.extend_from_slice(
            &u32::try_from(sei.len())
                .expect("fixture NAL fits")
                .to_be_bytes(),
        );
        unit.extend_from_slice(&sei);
        unit
    }

    /// An access unit with a coded slice and no SEI at all.
    fn plain_access_unit() -> Vec<u8> {
        let slice = [0x65_u8, 0x88, 0x88, 0x88];
        let mut unit = Vec::new();
        unit.extend_from_slice(&u32::try_from(slice.len()).expect("fits").to_be_bytes());
        unit.extend_from_slice(&slice);
        unit
    }

    /// An AVCC access unit carrying one DTVCC (CEA-708) caption SEI.
    fn dtvcc_access_unit() -> Vec<u8> {
        // A DTVCC packet start and continuation, and no Line 21 triplets.
        let payload: &[u8] = &[
            0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xC2, 0xFF, 0xFF, 0x02, 0x21, 0xFE,
            0x41, 0x42, 0xFF,
        ];
        let mut sei = vec![0x06, 0x04];
        sei.push(u8::try_from(payload.len()).expect("fixture payload fits"));
        sei.extend_from_slice(payload);
        sei.push(0x80);

        let mut unit = Vec::new();
        unit.extend_from_slice(
            &u32::try_from(sei.len())
                .expect("fixture NAL fits")
                .to_be_bytes(),
        );
        unit.extend_from_slice(&sei);
        unit
    }

    #[test]
    fn a_single_captioned_video_track_is_declared() {
        let tracks = [video_track(0)];
        let mut verifier = CaptionVerifier::new(&tracks, Some(Arc::from("en")));

        let declared = verifier
            .inspect(TrackId(0), &captioned_access_unit())
            .expect("the first observation declares");
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].channel, CaptionChannel::Cea608Field(0));
        assert_eq!(declared[0].language.as_deref(), Some("en"));
        assert_eq!(verifier.reconciliation(), CaptionReconciliation::Consistent);
    }

    #[test]
    fn a_ladder_carrying_captions_on_only_one_rendition_declares_nothing() {
        // The failure HLS cannot express: a client switching to the second
        // rendition would lose captions the playlist promised, so nothing is
        // declared until every video track carries them.
        let tracks = [video_track(0), video_track(1)];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        assert!(
            verifier
                .inspect(TrackId(0), &captioned_access_unit())
                .is_none()
        );
        assert!(verifier.inspect(TrackId(1), &plain_access_unit()).is_none());
        assert_eq!(
            verifier.reconciliation(),
            CaptionReconciliation::PartialLadder
        );
        assert_eq!(verifier.carriage(), (1, 2));
    }

    #[test]
    fn a_declaration_waits_for_every_video_track_to_be_confirmed() {
        let tracks = [video_track(0), video_track(1)];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        assert!(
            verifier
                .inspect(TrackId(0), &captioned_access_unit())
                .is_none()
        );
        let declared = verifier
            .inspect(TrackId(1), &captioned_access_unit())
            .expect("the ladder is now consistent");
        assert_eq!(declared.len(), 1);
        assert_eq!(verifier.reconciliation(), CaptionReconciliation::Consistent);
        assert_eq!(verifier.carriage(), (2, 2));
    }

    #[test]
    fn a_language_is_optional_on_the_declaration() {
        // HLS makes LANGUAGE optional on EXT-X-MEDIA, so a stream whose
        // container declares no language still gets its captions surfaced.
        let tracks = [video_track(0)];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        let declared = verifier
            .inspect(TrackId(0), &captioned_access_unit())
            .expect("declared without a language");
        assert!(declared[0].language.is_none());
        assert_eq!(declared[0].name.as_ref(), "CC1");
    }

    #[test]
    fn an_uncaptioned_publication_declares_nothing() {
        let tracks = [video_track(0)];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        assert!(verifier.inspect(TrackId(0), &plain_access_unit()).is_none());
        assert_eq!(verifier.reconciliation(), CaptionReconciliation::Absent);
    }

    #[test]
    fn a_ladder_carrying_different_channels_declares_nothing() {
        // Both renditions carry captions, so this is not a partial ladder — but
        // they share no channel, so there is nothing the presentation can
        // promise a client that switches between them.
        let tracks = [video_track(0), video_track(1)];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        assert!(
            verifier
                .inspect(TrackId(0), &captioned_access_unit())
                .is_none()
        );
        assert!(verifier.inspect(TrackId(1), &dtvcc_access_unit()).is_none());

        assert_eq!(
            verifier.reconciliation(),
            CaptionReconciliation::ChannelMismatch
        );
        assert!(verifier.declared_channels().is_empty());
        // Every track carries something, which is what separates this from a
        // partial ladder.
        assert_eq!(verifier.carriage(), (2, 2));
    }

    #[test]
    fn only_the_first_service_of_a_group_is_default() {
        // At most one member of an EXT-X-MEDIA group may be DEFAULT=YES, so a
        // publisher carrying both 608 and 708 must still produce one default.
        let tracks = [video_track(0)];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        verifier.inspect(TrackId(0), &captioned_access_unit());
        let declared = verifier
            .inspect(TrackId(0), &dtvcc_access_unit())
            .expect("the second channel changes the declaration");

        assert_eq!(declared.len(), 2);
        assert!(declared[0].is_default);
        assert_eq!(
            declared.iter().filter(|service| service.is_default).count(),
            1
        );
        assert!(declared.iter().all(|service| service.autoselect));
    }

    #[test]
    fn an_hevc_track_is_declared() {
        let tracks = [video_track_with(
            0,
            crate::domain::Codec::Hevc,
            hvcc_extradata(),
        )];
        let mut verifier = CaptionVerifier::new(&tracks, Some(Arc::from("en")));

        let declared = verifier
            .inspect(TrackId(0), &hevc_captioned_access_unit())
            .expect("HEVC captions declare the same services as H.264");
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].channel, CaptionChannel::Cea608Field(0));
        assert_eq!(verifier.reconciliation(), CaptionReconciliation::Consistent);
    }

    #[test]
    fn a_mixed_h264_and_hevc_ladder_declares_when_both_carry_the_same_channel() {
        let tracks = [
            video_track(0),
            video_track_with(1, crate::domain::Codec::Hevc, hvcc_extradata()),
        ];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        assert!(
            verifier
                .inspect(TrackId(0), &captioned_access_unit())
                .is_none()
        );
        let declared = verifier
            .inspect(TrackId(1), &hevc_captioned_access_unit())
            .expect("the mixed ladder is consistent");
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].channel, CaptionChannel::Cea608Field(0));
        assert_eq!(verifier.reconciliation(), CaptionReconciliation::Consistent);
        assert_eq!(verifier.carriage(), (2, 2));
    }

    #[test]
    fn an_av1_rendition_keeps_the_ladder_unverifiable() {
        let tracks = [
            video_track(0),
            video_track_with(1, crate::domain::Codec::Av1, Vec::new()),
        ];
        let mut verifier = CaptionVerifier::new(&tracks, None);

        assert!(
            verifier
                .inspect(TrackId(0), &captioned_access_unit())
                .is_none()
        );
        assert_eq!(
            verifier.reconciliation(),
            CaptionReconciliation::PartialLadder
        );
        assert_eq!(verifier.carriage(), (1, 2));
    }
}

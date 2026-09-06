//! Playlist values derived from the locked plan rather than from what arrived.
//!
//! Everything here is a function of a rendition's frozen [`PlaylistContract`]
//! and the presentation it belongs to. Nothing is inferred from observed media:
//! a value learned from the first segment is a value that changes when the
//! second one differs, and HLS treats several of these as promises for a
//! playlist's whole life.

use std::time::{Duration, SystemTime};

use crate::{
    delivery::hls::{
        DurationRule, PlaylistContract, TargetDurationMultiple, manifest::ServerControl,
    },
    domain::{TickTimestamp, Timebase},
};

/// Presentation-wide timing and timeout policy for HLS delivery.
///
/// HLS requires that *every* media playlist of one multivariant presentation
/// carry an identical `EXT-X-SERVER-CONTROL`, so the advertised values are
/// multiples of the presentation's widest cadence rather than of each
/// rendition's own. Request deadlines live beside them so all delivery timing
/// defaults are visible in one configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryTimingPolicy {
    /// Multiple of the longest target duration; Apple requires at least 3.
    pub hold_back: TargetDurationMultiple,
    /// How far behind the live edge a player is told to start.
    ///
    /// The floor on live-edge latency, and the one delivery timing value an
    /// operator sets: below roughly three part durations a client on a lossy
    /// link runs out of buffered parts and stalls, above it every viewer waits
    /// longer than they need to, and which way to err depends on the audience's
    /// network rather than on anything this node can measure.
    ///
    /// A [`DurationRule`] so it takes either the multiple form, which tracks a
    /// retuned part cadence, or an absolute a deployment pins for its own
    /// reasons.
    pub part_hold_back: DurationRule,
    pub can_block_reload: bool,
}

impl Default for DeliveryTimingPolicy {
    fn default() -> Self {
        Self {
            hold_back: TargetDurationMultiple::integer(3),
            part_hold_back: DurationRule::MultipleOfTarget(TargetDurationMultiple::integer(3)),
            can_block_reload: true,
        }
    }
}

/// The `EXT-X-SERVER-CONTROL` every playlist in this presentation must carry.
///
/// Derived once across renditions, never per playlist. A presentation whose
/// audio and video advertised different hold-backs would be malformed even
/// though each value was individually reasonable, so the widest cadence present
/// sets the value for all of them.
///
/// `CAN-SKIP-UNTIL` is six times the widest target duration — the protocol
/// floor — and is derived from `segment` the same way hold-back is. Advertising
/// it commits the origin to rendering `EXT-X-SKIP`; that rendering ships with
/// this value. `CAN-SKIP-DATERANGES` stays off (omitted): `NO` is not a spec
/// value, and well-behaved clients will not send `_HLS_skip=v2` without `YES`.
pub fn server_control(
    contracts: impl IntoIterator<Item = PlaylistContract>,
    policy: DeliveryTimingPolicy,
) -> Option<ServerControl> {
    let mut longest_target = None::<Duration>;
    let mut longest_part_target = None::<Duration>;
    for contract in contracts {
        let target = Duration::from_secs(contract.target_duration.get());
        longest_target = Some(longest_target.map_or(target, |current| current.max(target)));
        // Only renditions that publish parts contribute a part target. A
        // presentation mixing chunked video with segment-only WebVTT still
        // needs exactly one PART-HOLD-BACK, sized by the ones that have parts.
        if let Some(part_target) = contract.part_target {
            longest_part_target =
                Some(longest_part_target.map_or(part_target, |current| current.max(part_target)));
        }
    }

    let longest_target = longest_target?;
    Some(ServerControl {
        hold_back: Some(policy.hold_back.apply(longest_target)),
        part_hold_back: longest_part_target.map(|target| policy.part_hold_back.resolve(target)),
        can_block_reload: policy.can_block_reload,
        can_skip_until: Some(TargetDurationMultiple::integer(6).apply(longest_target)),
        can_skip_dateranges: false,
    })
}

/// How long a blocking playlist reload may be left unsatisfied.
///
/// Derived from the advertised hold-back rather than configured beside it. A
/// client told to sit `part_hold_back` behind the edge will request a part
/// that far ahead, so a deadline shorter than the hold-back would expire on
/// exactly the requests this origin invited. Written as two independent
/// constants the two agreed by coincidence, and nothing would have told the
/// next person retuning either that the pair was load-bearing.
///
/// Three target durations is the protocol's own floor; the hold-back plus one
/// part is what makes an invited request satisfiable. The wider of the two
/// wins, after which the request is answered with a temporary failure rather
/// than held indefinitely.
pub fn blocking_reload_deadline(
    contract: PlaylistContract,
    policy: DeliveryTimingPolicy,
) -> Duration {
    let target = Duration::from_secs(contract.target_duration.get());
    let protocol_floor = TargetDurationMultiple::integer(3).apply(target);
    let Some(part_target) = contract.part_target else {
        // Without parts there is no part hold-back to outlive.
        return protocol_floor;
    };
    protocol_floor.max(
        policy
            .part_hold_back
            .resolve(part_target)
            .saturating_add(part_target),
    )
}

/// The wall-clock time at which media starting at `media_start` is presented.
///
/// `media_start` is signed and may precede its publication's anchor: audio
/// priming puts the first encoded access unit before the presentation origin,
/// and a segmentation origin can sit earlier still. Both directions are
/// handled, and a time falling outside `SystemTime` is reported absent rather
/// than clamped to a wrong instant.
pub fn program_date_time(
    anchor: SystemTime,
    media_start: TickTimestamp,
    timebase: Timebase,
) -> Option<SystemTime> {
    match u64::try_from(media_start) {
        Ok(forward) => anchor.checked_add(timebase.ticks_to_duration(forward)),
        Err(_) => anchor.checked_sub(timebase.ticks_to_duration(media_start.unsigned_abs())),
    }
}

#[cfg(test)]
mod tests {
    use crate::mux::fixtures::config;

    use super::*;

    fn contract(segment_ticks: u64, chunk_ticks: Option<u64>) -> PlaylistContract {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(1));
        PlaylistContract::derive(&config(timebase, segment_ticks, chunk_ticks))
    }

    #[test]
    fn one_server_control_covers_the_widest_cadence_in_the_presentation() {
        let control = server_control(
            [
                contract(4, Some(1)),
                contract(6, Some(2)),
                // Segment-only WebVTT contributes a target but no part target.
                contract(6, None),
            ],
            DeliveryTimingPolicy::default(),
        )
        .expect("an active presentation has a server control");

        assert_eq!(control.hold_back, Some(Duration::from_secs(18)));
        assert_eq!(control.part_hold_back, Some(Duration::from_secs(6)));
        assert!(control.can_block_reload);
        assert_eq!(
            control.can_skip_until,
            Some(Duration::from_secs(36)),
            "six times the widest target, the protocol floor for a skip boundary"
        );
        assert!(
            !control.can_skip_dateranges,
            "an explicit CAN-SKIP-DATERANGES=NO is not a spec value"
        );
    }

    #[test]
    fn a_presentation_without_parts_advertises_no_part_hold_back() {
        let control = server_control([contract(6, None)], DeliveryTimingPolicy::default())
            .expect("a segment-only presentation still has a server control");

        assert_eq!(control.part_hold_back, None);
        assert_eq!(control.hold_back, Some(Duration::from_secs(18)));
    }

    #[test]
    fn a_presentation_with_no_renditions_has_nothing_to_control() {
        assert_eq!(server_control([], DeliveryTimingPolicy::default()), None);
    }

    #[test]
    fn the_blocking_deadline_never_expires_on_a_request_the_hold_back_invites() {
        // A client told to sit three parts behind the edge asks for a part that
        // far ahead. The deadline has to outlive that request, so it tracks the
        // hold-back rather than sitting beside it as an independent constant.
        let policy = DeliveryTimingPolicy {
            part_hold_back: DurationRule::Fixed(Duration::from_secs(30)),
            ..DeliveryTimingPolicy::default()
        };

        assert_eq!(
            blocking_reload_deadline(contract(6, Some(1)), policy),
            Duration::from_secs(31),
            "the hold-back plus one part, once that exceeds the protocol floor"
        );
    }

    #[test]
    fn the_blocking_deadline_keeps_the_protocol_floor_for_a_short_hold_back() {
        assert_eq!(
            blocking_reload_deadline(contract(6, Some(1)), DeliveryTimingPolicy::default()),
            Duration::from_secs(18),
            "three target durations, which a three-part hold-back does not reach"
        );
    }

    #[test]
    fn a_segment_only_presentation_blocks_on_the_protocol_floor_alone() {
        assert_eq!(
            blocking_reload_deadline(contract(6, None), DeliveryTimingPolicy::default()),
            Duration::from_secs(18),
            "with no parts there is no part hold-back to outlive"
        );
    }
    #[test]
    fn program_date_time_handles_media_beginning_before_its_anchor() {
        let anchor = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let timebase = Timebase::hz90k();

        assert_eq!(
            program_date_time(anchor, 90_000, timebase),
            Some(anchor + Duration::from_secs(1))
        );
        assert_eq!(
            program_date_time(anchor, -45_000, timebase),
            Some(anchor - Duration::from_millis(500)),
            "priming puts the first encoded unit before the presentation origin"
        );
        assert_eq!(program_date_time(anchor, 0, timebase), Some(anchor));
    }
}

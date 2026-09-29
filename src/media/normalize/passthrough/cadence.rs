//! Presentation-order validation gates decode-order release.
use super::{compensation::CompensationBudget, processing};
use crate::{
    domain::{
        CadenceSource, CadenceUnavailable, Codec, CompensationStatus, DiscoveredTrack, InputMode,
        MediaKind, NormalizationNotice, RecoveryMethod, RecoveryRejection, RecoveryTransition,
        Timebase, TimestampField, TimestampIssue, TimestampIssueCode, VideoCadence,
    },
    media::NormalizeError,
    source::Packet,
};
use std::collections::VecDeque;

pub(super) struct CadenceValidator {
    output_timebase: Timebase,
    picture_mapping: Option<crate::media::picture_mapping::PictureMapping>,
    input_timebase: Timebase,
    accounting_scale: i128,
    mode: InputMode,
    declaration: VideoCadence,
    status: CompensationStatus,
    depth: usize,
    queue: VecDeque<(Packet, bool)>,
    // Rational source-clock interval and anchored expected position.
    interval: i128,
    expected: Option<i128>,
    tolerance: u64,
    last: Option<i64>,
    clean_start: Option<i64>,
    budget: CompensationBudget,
    pub notices: Vec<NormalizationNotice>,
}
impl CadenceValidator {
    /// Whether a hole can be served as a GAP instead of ending the publication.
    ///
    /// Each codec qualifies only under the scope its declaration was verified
    /// for, because that scope is what makes "one packet, one displayed
    /// picture, one interval" true: H.264 progressive frames, HEVC progressive
    /// pictures on the base temporal layer, and AV1 single-layer temporal
    /// units. Presentation reordering is excluded for all three: with a
    /// reorder depth, a late picture cannot be told from a missing one until
    /// pictures after it have already been released in decode order.
    fn supports_gaps(&self) -> bool {
        use crate::domain::CadenceScope as S;
        self.depth == 0
            && matches!(
                (self.status.codec, self.declaration),
                (
                    Codec::H264,
                    VideoCadence::Fixed {
                        scope: S::ProgressiveFrames,
                        ..
                    }
                ) | (
                    Codec::Hevc,
                    VideoCadence::Fixed {
                        scope: S::ProgressiveBaseLayer,
                        ..
                    }
                ) | (
                    Codec::Av1,
                    VideoCadence::Fixed {
                        scope: S::SingleLayerTemporalUnits,
                        ..
                    }
                )
            )
    }

    /// Preview without accounting or notices. The caller validates decode
    /// timing and representability before `push` commits the cadence decision.
    pub fn gap_before(
        &self,
        packet: &Packet,
    ) -> Result<Option<crate::media::MissingInterval>, NormalizeError> {
        if !self.supports_gaps() {
            return Ok(None);
        }
        let output = self.output_timebase;
        if self.mode == InputMode::Strict || !matches!(self.declaration, VideoCadence::Fixed { .. })
        {
            return Ok(None);
        }
        if self
            .picture_mapping
            .as_ref()
            .is_some_and(|mapping| mapping.check(packet.payload.as_bytes()).is_err())
        {
            return Ok(None);
        }
        // An AV1 temporal unit that does not show exactly one frame has no
        // presentation interval of its own to measure a hole against; `push`
        // decides what it means.
        if self.status.codec == Codec::Av1
            && crate::media::av1::displayed_pictures(packet.payload.as_bytes()) != Some(1)
        {
            return Ok(None);
        }
        let (Some(expected), Some(actual)) = (self.expected, packet.pts) else {
            return Ok(None);
        };
        let scaled = i128::from(actual)
            .checked_mul(self.accounting_scale)
            .ok_or_else(|| processing("cadence timestamp overflow"))?;
        let excess = scaled
            .checked_sub(expected)
            .ok_or_else(|| processing("cadence difference overflow"))?;
        if excess <= i128::from(self.tolerance) {
            return Ok(None);
        }
        let expected = i64::try_from(expected).map_err(|_| processing("gap endpoint overflow"))?;
        let start = crate::domain::TimebaseProjection::new(self.status.timebase, output)
            .timestamp(expected)
            .ok_or_else(|| processing("gap projection overflow"))?;
        let end = crate::domain::TimebaseProjection::new(self.input_timebase, output)
            .timestamp(actual)
            .ok_or_else(|| processing("gap projection overflow"))?;
        if end <= start {
            return Err(processing("video gap is not representable"));
        }
        Ok(Some(crate::media::MissingInterval {
            track_id: self.status.track,
            media_kind: MediaKind::Video,
            start,
            end,
            timebase: output,
        }))
    }

    pub fn declaration(&self) -> VideoCadence {
        self.declaration
    }
    pub fn new(
        track: &DiscoveredTrack,
        output: Timebase,
        mode: InputMode,
        declaration: VideoCadence,
        depth: usize,
    ) -> Result<Self, NormalizeError> {
        // A common clock represents both the source timestamps and rational
        // picture interval exactly, including fractional excess durations.
        let (mut a, mut b) = (
            u64::from(track.timebase.den().get()),
            declaration
                .rate()
                .map_or(1, |r| u64::from(r.numerator().get())),
        );
        let product = a * b;
        while b != 0 {
            (a, b) = (b, a % b);
        }
        let common = u32::try_from(product / a)
            .ok()
            .and_then(std::num::NonZeroU32::new);
        let declaration = if common.is_none() && matches!(declaration, VideoCadence::Fixed { .. }) {
            VideoCadence::Unverifiable {
                rate: declaration.rate(),
                source: declaration.source().expect("fixed source"),
                reason: CadenceUnavailable::InvalidTiming,
            }
        } else {
            declaration
        };
        let clock = Timebase::new(nz::u32!(1), common.unwrap_or(track.timebase.den()));
        let accounting_scale = i128::from(clock.den().get())
            * i128::from(track.timebase.num().get())
            / i128::from(track.timebase.den().get());
        let interval = declaration.rate().map_or(0, |rate| {
            i128::from(rate.denominator().get()) * i128::from(clock.den().get())
                / i128::from(rate.numerator().get())
        });
        // Compare the sum of source precision and one output-clock tick
        // exactly; rounding each term to source ticks would double tolerance.
        let tolerance_denominator = i128::from(output.den().get());
        let tolerance_numerator = accounting_scale * tolerance_denominator
            + i128::from(output.num().get()) * i128::from(clock.den().get());
        let tolerance = u64::try_from(tolerance_numerator / tolerance_denominator)
            .map_err(|_| processing("cadence tolerance overflow"))?;
        let declaration = if matches!(declaration, VideoCadence::Fixed { .. })
            && interval <= i128::from(tolerance)
        {
            VideoCadence::Unverifiable {
                rate: declaration.rate(),
                source: declaration.source().expect("fixed source"),
                reason: CadenceUnavailable::TimestampPrecision,
            }
        } else {
            declaration
        };
        let mut this = Self {
            output_timebase: output,
            picture_mapping: crate::media::picture_mapping::PictureMapping::new(
                track.codec,
                track.codec_extradata.as_bytes(),
            ),
            input_timebase: track.timebase,
            accounting_scale,
            mode,
            declaration,
            depth,
            queue: VecDeque::new(),
            interval,
            expected: None,
            tolerance,
            last: None,
            clean_start: None,
            budget: CompensationBudget::new(track.timebase, clock),
            notices: Vec::new(),
            status: CompensationStatus {
                media_kind: MediaKind::Video,
                cadence: Some(declaration),
                track: track.id,
                codec: track.codec,
                method: RecoveryMethod::Gap,
                timebase: clock,
                missing_ticks: 0,
                replacement_ticks: 0,
                episode_holes: 0,
                episode_ticks: 0,
                total_holes: 0,
                total_ticks: 0,
                degraded: false,
            },
        };
        match declaration {
            VideoCadence::Conflicting { .. } => {
                return Err(this.error(TimestampIssueCode::VideoCadenceConflict, 0, 0));
            }
            VideoCadence::Unverifiable { .. } => this.unavailable(declaration)?,
            _ => {}
        }
        Ok(this)
    }
    fn error(&self, code: TimestampIssueCode, expected: i128, actual: i128) -> NormalizeError {
        NormalizeError::Timestamp(Box::new(TimestampIssue {
            cadence: Some(self.declaration),
            code,
            track: self.status.track,
            media_kind: MediaKind::Video,
            codec: self.status.codec,
            field: TimestampField::Pts,
            reference: expected,
            actual,
            timebase: self.status.timebase,
            tolerance_ticks: Some(self.tolerance),
            maximum: None,
            missing_ticks: None,
            recovery_rejection: None,
        }))
    }
    fn unavailable(&mut self, declaration: VideoCadence) -> Result<(), NormalizeError> {
        self.declaration = declaration;
        self.status.cadence = Some(declaration);
        if self.mode == InputMode::Strict {
            return Err(self.error(TimestampIssueCode::VideoCadenceUnavailable, 0, 0));
        }
        self.status.method = RecoveryMethod::UnverifiedCadence;
        self.status.degraded = true;
        self.notices.push(NormalizationNotice {
            transition: RecoveryTransition::Unavailable,
            status: self.status.clone(),
        });
        Ok(())
    }
    pub fn push(&mut self, packet: Packet) -> Result<Vec<Packet>, NormalizeError> {
        if !matches!(self.declaration, VideoCadence::Fixed { .. }) {
            return Ok(vec![packet]);
        }
        if let Some(mapping) = &self.picture_mapping
            && let Err(reason) = mapping.check(packet.payload.as_bytes())
        {
            self.unavailable(VideoCadence::Unverifiable {
                rate: self.declaration.rate(),
                source: self.declaration.source().expect("fixed source"),
                reason,
            })?;
            let mut ready: Vec<_> = self.queue.drain(..).map(|(p, _)| p).collect();
            ready.push(packet);
            return Ok(ready);
        }
        let mut displayed = true;
        if self.status.codec == Codec::Av1 {
            match crate::media::av1::displayed_pictures(packet.payload.as_bytes()) {
                Some(0) => displayed = false,
                Some(1) => {}
                _ => {
                    self.unavailable(VideoCadence::Unverifiable {
                        rate: self.declaration.rate(),
                        source: CadenceSource::Av1Sequence,
                        reason: CadenceUnavailable::DisplayMapping,
                    })?;
                    let mut ready: Vec<_> = self.queue.drain(..).map(|(p, _)| p).collect();
                    ready.push(packet);
                    return Ok(ready);
                }
            }
        }
        if packet.pts.is_none() {
            return Err(processing("video packet has no PTS"));
        }
        self.queue.push_back((packet, !displayed));
        // A future reference picture can precede many B-pictures in decode
        // order even when only one picture awaits presentation. Bound retained
        // decode packets separately from the declared presentation reorder depth.
        if self.queue.len() > 132 {
            return Err(processing(
                "cadence validation exceeds the retained packet limit",
            ));
        }
        if self.queue.iter().filter(|(_, done)| !done).count() > self.depth {
            self.check_next()?;
        }
        Ok(self.drain())
    }
    pub fn finish(&mut self) -> Result<Vec<Packet>, NormalizeError> {
        while self.queue.iter().any(|(_, done)| !done) {
            self.check_next()?;
        }
        Ok(self.drain())
    }
    fn drain(&mut self) -> Vec<Packet> {
        let mut ready = Vec::new();
        while self.queue.front().is_some_and(|(_, done)| *done) {
            ready.push(self.queue.pop_front().expect("front exists").0);
        }
        ready
    }
    fn check_next(&mut self) -> Result<(), NormalizeError> {
        let index = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, (_, done))| !done)
            .min_by_key(|(_, (p, _))| p.pts)
            .map(|(i, _)| i)
            .expect("pending picture");
        let actual = self.queue[index].0.pts.expect("validated PTS");
        let scaled = i128::from(actual)
            .checked_mul(self.accounting_scale)
            .ok_or_else(|| processing("cadence timestamp overflow"))?;
        let mut anchor = scaled;
        if let Some(expected) = self.expected {
            let delta = scaled
                .checked_sub(expected)
                .ok_or_else(|| processing("cadence difference overflow"))?;
            let outside_tolerance = delta.unsigned_abs() > u128::from(self.tolerance);
            if (delta < 0 || self.mode == InputMode::Strict) && outside_tolerance {
                return Err(self.error(
                    TimestampIssueCode::VideoCadenceViolation,
                    expected,
                    scaled,
                ));
            }
            if delta > 0 && outside_tolerance {
                // The common clock keeps budget accounting exact.
                let ticks =
                    u64::try_from(delta).map_err(|_| processing("cadence excess overflow"))?;
                self.compensate(actual, ticks)?;
            } else {
                anchor = expected;
                if self.status.degraded {
                    let start = *self.clean_start.get_or_insert(self.last.unwrap_or(actual));
                    if self
                        .budget
                        .clean(actual.abs_diff(start), self.input_timebase)
                    {
                        self.status.degraded = false;
                        self.status.missing_ticks = 0;
                        self.status.replacement_ticks = 0;
                        self.notices.push(NormalizationNotice {
                            transition: RecoveryTransition::Recovered,
                            status: self.status.clone(),
                        });
                    }
                }
            }
        }
        self.expected = Some(
            anchor
                .checked_add(self.interval)
                .ok_or_else(|| processing("cadence grid overflow"))?,
        );
        self.last = Some(actual);
        self.queue[index].1 = true;
        Ok(())
    }
    fn compensate(&mut self, end: i64, ticks: u64) -> Result<(), NormalizeError> {
        // Never fall back to stretching a frame when a GAP cannot be emitted.
        let eligibility = if self.supports_gaps() {
            self.budget.check(end, ticks)
        } else {
            Err(RecoveryRejection::UnsupportedConfiguration)
        };
        if let Err(reason) = eligibility {
            let mut error = self.error(
                TimestampIssueCode::VideoCadenceViolation,
                self.expected.unwrap_or_default(),
                i128::from(end) * self.accounting_scale,
            );
            if let NormalizeError::Timestamp(issue) = &mut error {
                issue.recovery_rejection = Some(reason);
                issue.missing_ticks = Some(ticks);
            }
            return Err(error);
        }
        let transition = if self.status.degraded {
            RecoveryTransition::Compensated
        } else {
            self.status.episode_holes = 0;
            self.status.episode_ticks = 0;
            RecoveryTransition::Degraded
        };
        self.status.total_ticks = self
            .status
            .total_ticks
            .checked_add(ticks)
            .ok_or_else(|| processing("cadence total overflow"))?;
        self.status.total_holes = self
            .status
            .total_holes
            .checked_add(1)
            .ok_or_else(|| processing("cadence count overflow"))?;
        self.status.episode_ticks = self
            .status
            .episode_ticks
            .checked_add(ticks)
            .ok_or_else(|| processing("cadence episode overflow"))?;
        self.status.episode_holes += 1;
        self.status.missing_ticks = ticks;
        self.status.replacement_ticks = 0;
        self.status.degraded = true;
        self.clean_start = None;
        self.budget.commit(end, ticks);
        self.notices.push(NormalizationNotice {
            transition,
            status: self.status.clone(),
        });
        Ok(())
    }
}

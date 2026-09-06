//! Opus roll recovery in access-unit counts, including variable packet durations.

use broadcast_common::Serialize;
use std::collections::VecDeque;
use transmux::sample_groups::GROUPING_TYPE_ROLL;
use transmux::{
    MovieFragmentBox, Sample, SampleGroupDescriptionBox, SampleTableBox, SampleToGroupBox,
    SbgpEntry, SgpdEntry, StblChild,
};

const PREROLL: u32 = 3_840;
const MAX_PACKETS: u32 = 32; // 80 ms / Opus's shortest packet (2.5 ms).

#[derive(Default)]
pub struct RollRecovery {
    preceding: VecDeque<u32>,
    duration: u32,
}

impl RollRecovery {
    pub fn init(table: &mut SampleTableBox) {
        let descriptions = SampleGroupDescriptionBox {
            version: 1,
            flags: 0,
            grouping_type: GROUPING_TYPE_ROLL,
            default_length: 2,
            entries: (1..=32)
                .map(|count| SgpdEntry::Roll {
                    roll_distance: -count,
                })
                .collect(),
        };
        table
            .children
            .push(StblChild::Opaque(descriptions.to_bytes()));
        table
            .children
            .push(StblChild::Opaque(groups(Vec::new()).to_bytes()));
    }

    pub fn fragment(&mut self, bytes: &[u8], samples: &[Sample]) -> Result<Vec<u8>, Box<str>> {
        let mut entries: Vec<SbgpEntry> = Vec::new();
        for sample in samples {
            // trun may shorten the final packet for end trimming; recovery counts
            // the full decoded packet duration.
            let duration = crate::media::opus::packet_samples(&sample.data)?;
            if !(120..=5_760).contains(&duration) {
                return Err("invalid Opus packet duration for roll recovery".into());
            }
            // Before 80 ms of history exists, use a conservative description.
            // At stream start the edit list supplies priming; no earlier media exists.
            let count = if self.duration >= PREROLL {
                u32::try_from(self.preceding.len()).map_err(|_| "Opus roll count overflow")?
            } else {
                MAX_PACKETS
            };
            if let Some(last) = entries
                .last_mut()
                .filter(|last| last.group_description_index == count)
            {
                last.sample_count += 1;
            } else {
                entries.push(SbgpEntry {
                    sample_count: 1,
                    group_description_index: count,
                });
            }
            self.preceding.push_back(duration);
            self.duration += duration;
            while let Some(&first) = self.preceding.front() {
                if self.duration - first < PREROLL {
                    break;
                }
                self.duration -= first;
                self.preceding.pop_front();
            }
        }
        append_groups(bytes, &groups(entries).to_bytes())
    }
}

fn groups(entries: Vec<SbgpEntry>) -> SampleToGroupBox {
    SampleToGroupBox {
        version: 0,
        flags: 0,
        grouping_type: GROUPING_TYPE_ROLL,
        grouping_type_parameter: None,
        entries,
    }
}

/// The upstream fragment model discards extension children. Serialize its
/// timing boxes, then attach sbgp and adjust trun's moof-relative mdat offset.
fn append_groups(bytes: &[u8], groups: &[u8]) -> Result<Vec<u8>, Box<str>> {
    let mut remaining = bytes;
    let mut out = Vec::with_capacity(bytes.len() + groups.len());
    while !remaining.is_empty() {
        let (item, size) = transmux::parse_box(remaining).map_err(error)?;
        if item.header.box_type.is(b"moof") {
            let mut moof = MovieFragmentBox::parse_body(item.body).map_err(error)?;
            if moof.traf.len() != 1 {
                return Err("Opus rendition must have one fragment track".into());
            }
            let delta = i32::try_from(groups.len()).map_err(|_| "roll metadata size overflow")?;
            for run in &mut moof.traf[0].trun {
                let offset = run
                    .data_offset
                    .as_mut()
                    .ok_or("fragment has no relative data offset")?;
                *offset = offset
                    .checked_add(delta)
                    .ok_or("fragment data offset overflow")?;
            }
            let mut traf = moof.traf[0].to_bytes();
            traf.extend_from_slice(groups);
            set_size(&mut traf)?;
            let mut body = moof.mfhd.to_bytes();
            body.extend_from_slice(&traf);
            let mut container = vec![0, 0, 0, 0, b'm', b'o', b'o', b'f'];
            container.extend_from_slice(&body);
            set_size(&mut container)?;
            out.extend_from_slice(&container);
        } else {
            out.extend_from_slice(&remaining[..size]);
        }
        remaining = &remaining[size..];
    }
    Ok(out)
}

fn set_size(bytes: &mut [u8]) -> Result<(), Box<str>> {
    let size = u32::try_from(bytes.len()).map_err(|_| "fragment box size overflow")?;
    bytes[..4].copy_from_slice(&size.to_be_bytes());
    Ok(())
}
fn error(error: impl std::fmt::Display) -> Box<str> {
    error.to_string().into_boxed_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roll_distances_use_history_across_fragment_boundaries() -> Result<(), Box<str>> {
        let mut recovery = RollRecovery::default();
        for (sequence, durations, expected) in [
            (1, vec![960; 5], vec![(4, 32), (1, 4)]),
            (2, vec![2880, 960, 120], vec![(1, 4), (2, 2)]),
        ] {
            let samples: Vec<_> = durations
                .into_iter()
                .map(|duration| {
                    Sample::new(
                        bytes::Bytes::from(vec![
                            match duration {
                                120 => 0x80,
                                2880 => 0x18,
                                _ => 0xf8,
                            },
                            0xff,
                            0xfe,
                        ]),
                        Some(0),
                        Some(0),
                        Some(duration),
                        true,
                    )
                })
                .collect();
            let bytes = transmux::build_media_segment(
                sequence,
                &[transmux::FragmentTrackData::new(1, 0, &samples)],
            )
            .map_err(error)?;
            let bytes = recovery.fragment(&bytes, &samples)?;
            let mut found = None;
            for item in transmux::box_iter(&bytes) {
                let (item, _) = item.map_err(error)?;
                if item.header.box_type.is(b"moof") {
                    for child in transmux::box_iter(item.body) {
                        let (child, _) = child.map_err(error)?;
                        if child.header.box_type.is(b"traf") {
                            for group in transmux::box_iter(child.body) {
                                let (group, _) = group.map_err(error)?;
                                if group.header.box_type.is(b"sbgp") {
                                    found = Some(
                                        SampleToGroupBox::parse_body(group.body).map_err(error)?,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            assert_eq!(
                found
                    .unwrap()
                    .entries
                    .iter()
                    .map(|e| (e.sample_count, e.group_description_index))
                    .collect::<Vec<_>>(),
                expected
            );
        }
        Ok(())
    }
}

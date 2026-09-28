//! Opus roll recovery in access-unit counts, including variable packet durations.

use broadcast_common::Serialize;
use std::collections::VecDeque;
use transmux::sample_groups::GROUPING_TYPE_ROLL;
use transmux::{
    SampleGroupDescriptionBox, SampleTableBox, SampleToGroupBox, SbgpEntry, SgpdEntry, StblChild,
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

    /// Roll groups for one run of packets, advancing the history across runs.
    /// The fragment writer places the returned `sbgp` at the end of `traf`.
    pub fn groups<'a>(
        &mut self,
        packets: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<SampleToGroupBox, Box<str>> {
        let mut entries: Vec<SbgpEntry> = Vec::new();
        for packet in packets {
            // trun may shorten the final packet for end trimming; recovery counts
            // the full decoded packet duration.
            let duration = crate::media::opus::packet_samples(packet)?;
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
        Ok(groups(entries))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roll_distances_use_history_across_fragment_boundaries() -> Result<(), Box<str>> {
        let mut recovery = RollRecovery::default();
        for (durations, expected) in [
            (vec![960; 5], vec![(4, 32), (1, 4)]),
            (vec![2880, 960, 120], vec![(1, 4), (2, 2)]),
        ] {
            let packets: Vec<_> = durations
                .into_iter()
                .map(|duration| {
                    [
                        match duration {
                            120 => 0x80,
                            2880 => 0x18,
                            _ => 0xf8,
                        },
                        0xff,
                        0xfe,
                    ]
                })
                .collect();
            let groups = recovery.groups(packets.iter().map(<[u8; 3]>::as_slice))?;
            assert_eq!(
                groups
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

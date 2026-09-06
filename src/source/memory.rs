//! What one publisher costs this node before its media reaches the store.
//!
//! `[capacity] memory_per_stream` bounds *retained* parts and segments, which
//! is what it should mean to an operator. It says nothing about what a
//! publisher holds on the way there, and that is not small: transport framing,
//! the demuxer channel, an in-flight batch, pre-roll, and discovery probing all
//! retain bytes for the life of a session.
//!
//! Those stay compiled rather than becoming settings. They are properties of
//! the pipeline rather than policy, and an operator has no basis on which to
//! choose a value: the figure is driven by track count and group-of-pictures
//! structure, which belong to the publisher rather than the deployment. A knob
//! whose correct value is unknowable invites tuning that can only break
//! discovery for multi-rendition contributors.
//!
//! These budgets cover application payload buffers. The native demuxer also
//! retains per-PID PES/probe buffers, and parsed tags and queue entries add
//! allocation overhead. The sum is not a strict bound on process memory.

use bytesize::ByteSize;

/// Application payload budgets per publisher, excluding codec and allocator overhead.
pub struct PipelineMemory;

impl PipelineMemory {
    /// Encoded bytes buffered by a transport for the demuxer to read.
    pub const TRANSPORT: usize = 16 * 1024 * 1024;
    /// Demuxed payload allowed to wait in the worker channel.
    pub const DEMUX_QUEUE: usize = 16 * 1024 * 1024;
    /// One batch of packets in flight between the source and normalization.
    pub const BATCH: usize = 16 * 1024 * 1024;
    /// Media pre-roll retains while it looks for a keyframe cadence.
    ///
    /// The largest single term, and deliberately so. The horizon applies per
    /// track, so a multi-rendition publisher needs the headroom: pre-roll
    /// cannot lock until every video track has shown a compatible cadence, and
    /// samples are charged at retained cost rather than payload size, so many
    /// small access units consume it far faster than their bytes suggest.
    pub const PREROLL: usize = 64 * 1024 * 1024;
    /// Container probing before discovery concludes.
    pub const DISCOVERY: usize = 8 * 1024 * 1024;

    /// Sum of the application payload budgets outside the store.
    pub const TOTAL: usize =
        Self::TRANSPORT + Self::DEMUX_QUEUE + Self::BATCH + Self::PREROLL + Self::DISCOVERY;

    /// The budget in a form an operator-facing message can print.
    pub fn total() -> ByteSize {
        ByteSize::b(Self::TOTAL as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_per_publisher_budget_is_the_sum_of_what_the_stages_hold() {
        // The figure `config.md` quotes for capacity planning. It is asserted
        // rather than left implicit so that adding a buffer without accounting
        // for it fails here instead of silently understating the node.
        assert_eq!(
            PipelineMemory::TOTAL,
            PipelineMemory::TRANSPORT
                + PipelineMemory::DEMUX_QUEUE
                + PipelineMemory::BATCH
                + PipelineMemory::PREROLL
                + PipelineMemory::DISCOVERY
        );
        assert_eq!(PipelineMemory::total(), ByteSize::mib(120));
    }
}

//! Adapter safety limits outside shared publisher allocation accounting.
//!
//! Application payload ownership uses `domain::PipelineBudget`. These limits
//! bound individual protocol operations and uninstrumented dependency buffers;
//! their sum is not a memory ceiling. In particular, discovery counts bytes
//! examined rather than bytes retained.

pub struct PipelineMemory;

impl PipelineMemory {
    /// Native transport reassembly/cache safety limit, independent of ownership.
    pub const TRANSPORT: usize = 16 * 1024 * 1024;
    /// One source call's payload/work bound; not a reserved memory partition.
    pub const BATCH: usize = 16 * 1024 * 1024;
    /// Maximum input examined before container discovery must conclude.
    pub const DISCOVERY: usize = 8 * 1024 * 1024;
}

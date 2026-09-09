//! Sample-duration observations; admission policy is resolved separately.

use crate::domain::TickDuration;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccessUnitCadence {
    longest: TickDuration,
}

impl AccessUnitCadence {
    pub fn observe(&mut self, duration: TickDuration) {
        self.longest = self.longest.max(duration);
    }

    pub fn longest(self) -> TickDuration {
        self.longest
    }
}

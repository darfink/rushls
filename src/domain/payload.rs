use super::{BudgetExceeded, PipelineBudget, Reservation};
use bytes::Bytes;
use std::sync::Arc;

/// Reference-counted encoded bytes shared between pipeline stages.
///
/// Cloning is a refcount bump, so one buffer travels from the demuxer through
/// muxing into every concurrent HLS reader without being copied. Muxers should
/// build into a [`bytes::BytesMut`] and freeze it, which hands ownership over
/// without a final copy either.
#[derive(Clone, Debug, Default)]
pub struct Payload {
    data: Bytes,
    charge: Option<Arc<Reservation>>,
}

// Resource ownership is not part of media identity.
impl PartialEq for Payload {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}
impl Eq for Payload {}

struct ChargedBytes {
    data: Bytes,
    _charge: Arc<Reservation>,
}
impl AsRef<[u8]> for ChargedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Payload {
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        Self {
            data: bytes.into(),
            charge: None,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Escaping raw bytes retain the charge, including through slices.
    pub fn bytes(&self) -> Bytes {
        self.clone().into_bytes()
    }

    pub fn into_bytes(self) -> Bytes {
        match self.charge {
            Some(charge) => Bytes::from_owner(ChargedBytes {
                data: self.data,
                _charge: charge,
            }),
            None => self.data,
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn budget(&self) -> Option<&PipelineBudget> {
        self.charge.as_ref().map(|charge| charge.budget())
    }

    /// Admit a dependency-produced payload before retaining it. Dependency
    /// allocation itself is separately bounded; this cannot retroactively
    /// meter a backing allocation hidden behind a sliced `Bytes`.
    pub fn account(
        &mut self,
        budget: &PipelineBudget,
        overhead: usize,
        stage: &'static str,
    ) -> Result<(), BudgetExceeded> {
        if self.charge.is_none() {
            self.charge = Some(budget.charge_bytes(&self.data, overhead, stage)?);
        }
        Ok(())
    }

    /// Attach a reservation acquired before building an output allocation.
    pub fn reserved(data: Vec<u8>, reservation: Reservation) -> Self {
        Self::reserved_bytes(Bytes::from(data), reservation)
    }

    pub fn reserved_bytes(data: Bytes, reservation: Reservation) -> Self {
        let charge = reservation.budget().clone().register(&data, reservation);
        Self {
            data,
            charge: Some(charge),
        }
    }

    /// Delivery takes over this reference. Other pipeline references and raw
    /// byte views retain their own leases until their owners release them.
    #[must_use]
    pub fn into_retained(self) -> Self {
        Self {
            data: self.data,
            charge: None,
        }
    }
}

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}
impl From<&'static [u8]> for Payload {
    fn from(bytes: &'static [u8]) -> Self {
        Self::from_bytes(Bytes::from_static(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_slices_keep_the_reservation_alive() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(1024);
        let mut payload = Payload::from(vec![0; 1024]);
        payload.account(&budget, 0, "source")?;
        let slice = payload.bytes().slice(..1);
        drop(payload);
        assert_eq!(budget.used(), 1024);
        drop(slice);
        assert_eq!(budget.used(), 0);
        Ok(())
    }
    #[test]
    fn backing_is_freed_before_its_quota_is_returned() -> Result<(), BudgetExceeded> {
        struct ObservedOwner {
            bytes: [u8; 32],
            budget: PipelineBudget,
        }
        impl AsRef<[u8]> for ObservedOwner {
            fn as_ref(&self) -> &[u8] {
                &self.bytes
            }
        }
        impl Drop for ObservedOwner {
            fn drop(&mut self) {
                assert_eq!(self.budget.used(), 32);
            }
        }
        let budget = PipelineBudget::new(32);
        let data = Bytes::from_owner(ObservedOwner {
            bytes: [1; 32],
            budget: budget.clone(),
        });
        let mut payload = Payload::from_bytes(data);
        payload.account(&budget, 0, "demux")?;
        drop(payload);
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn adapter_slices_reuse_the_transport_allocation_charge() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(1024);
        let slab = Payload::reserved(vec![1; 1024], budget.try_reserve(1024, "RTMP receive")?);
        let mut packet = Payload::from_bytes(slab.bytes().slice(100..200));
        packet.account(&budget, 0, "demux")?;
        assert_eq!(budget.used(), 1024);
        assert_eq!(budget.origins(), [1024, 0, 0, 0, 0]);
        drop(slab);
        assert_eq!(budget.used(), 1024);
        drop(packet);
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn delivery_does_not_release_other_pipeline_references() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(128);
        let mut payload = Payload::from(vec![1; 128]);
        payload.account(&budget, 0, "mux")?;
        let alias = payload.clone();
        let stored = payload.into_retained();
        assert_eq!(budget.used(), 128);
        drop(alias);
        assert_eq!(budget.used(), 0);
        assert_eq!(stored.as_bytes(), &[1; 128]);
        Ok(())
    }
}

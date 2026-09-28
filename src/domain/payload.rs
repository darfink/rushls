use super::{BudgetExceeded, PipelineBudget, Reservation, Stage};
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
    charge: Option<Arc<Lease>>,
}

/// A shared allocation charge plus the charging owner's own overhead. The
/// allocation may be shared with other owners (slices of one slab); the
/// overhead belongs to this owner and its clones only.
#[derive(Debug)]
struct Lease {
    allocation: Arc<Reservation>,
    _overhead: Option<Reservation>,
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
    _charge: Arc<Lease>,
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
        self.charge
            .as_ref()
            .map(|charge| charge.allocation.budget())
    }

    /// Admit a dependency-produced payload before retaining it. Dependency
    /// allocation itself is separately bounded; this cannot retroactively
    /// meter a backing allocation hidden behind a sliced `Bytes`.
    ///
    /// `overhead` is charged even when the backing allocation already has a
    /// lease, because each owner's metadata is a distinct allocation.
    pub fn account(
        &mut self,
        budget: &PipelineBudget,
        overhead: usize,
        stage: Stage,
    ) -> Result<(), BudgetExceeded> {
        if self.charge.is_some() {
            return Ok(());
        }
        let overhead = (overhead > 0)
            .then(|| budget.try_reserve(overhead, stage))
            .transpose()?;
        let allocation = budget.charge_bytes(&self.data, stage)?;
        self.charge = Some(Arc::new(Lease {
            allocation,
            _overhead: overhead,
        }));
        Ok(())
    }

    /// Attach a reservation acquired before building an output allocation.
    pub fn reserved(data: Vec<u8>, reservation: Reservation) -> Self {
        Self::reserved_bytes(Bytes::from(data), reservation)
    }

    pub fn reserved_bytes(data: Bytes, reservation: Reservation) -> Self {
        let allocation = reservation.budget().clone().register(&data, reservation);
        Self {
            data,
            charge: Some(Arc::new(Lease {
                allocation,
                _overhead: None,
            })),
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
        payload.account(&budget, 0, Stage::Demux)?;
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
        payload.account(&budget, 0, Stage::Demux)?;
        drop(payload);
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn adapter_slices_reuse_the_transport_allocation_charge() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(1024);
        let slab = Payload::reserved(vec![1; 1024], budget.try_reserve(1024, Stage::RtmpReceive)?);
        let mut packet = Payload::from_bytes(slab.bytes().slice(100..200));
        packet.account(&budget, 0, Stage::Demux)?;
        assert_eq!(budget.used(), 1024);
        assert_eq!(budget.origins(), [1024, 0, 0, 0, 0]);
        drop(slab);
        assert_eq!(budget.used(), 1024);
        drop(packet);
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn reused_allocation_leases_still_charge_owner_overhead() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(4096);
        let slab = Payload::reserved(vec![1; 1024], budget.try_reserve(1024, Stage::RtmpReceive)?);
        let mut first = Payload::from_bytes(slab.bytes().slice(0..100));
        let mut second = Payload::from_bytes(slab.bytes().slice(100..200));
        first.account(&budget, 64, Stage::Demux)?;
        second.account(&budget, 64, Stage::Demux)?;
        // One slab charge, plus each packet's own overhead.
        assert_eq!(budget.used(), 1024 + 128);
        drop((slab, first));
        assert_eq!(budget.used(), 1024 + 64);
        drop(second);
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn delivery_does_not_release_other_pipeline_references() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(128);
        let mut payload = Payload::from(vec![1; 128]);
        payload.account(&budget, 0, Stage::MuxOutput)?;
        let alias = payload.clone();
        let stored = payload.into_retained();
        assert_eq!(budget.used(), 128);
        drop(alias);
        assert_eq!(budget.used(), 0);
        assert_eq!(stored.as_bytes(), &[1; 128]);
        Ok(())
    }
}

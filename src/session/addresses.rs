//! How many publishers one client address may hold at once.
//!
//! [`PendingPublishers`](super::PendingPublishers) bounds each listener's
//! pre-admission window, but not who fills it: one host that keeps connecting
//! and stalling can occupy every slot, and admission never gets to run for
//! anyone else. Counting per address closes that without asking the
//! admission service, which by then would already be the thing being flooded.
//!
//! A permit covers the connection's whole life, pending and admitted, and is
//! shared by every ingest listener: the limit is about the client, not the
//! protocol it happened to pick.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    num::NonZeroUsize,
    sync::Arc,
};

use parking_lot::Mutex;

/// The unit one client is counted as.
///
/// IPv6 is grouped by `/64`: a single host is routinely handed a whole
/// prefix, so counting individual addresses would be free to evade.
/// IPv4-mapped IPv6 addresses count as the IPv4 address they carry, which is
/// how a dual-stack socket reports an IPv4 peer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, derive_more::Display)]
pub enum AddressKey {
    #[display("{_0}")]
    V4(std::net::Ipv4Addr),
    #[display("{_0}/64")]
    V6Prefix(Ipv6Addr),
}

impl From<IpAddr> for AddressKey {
    fn from(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(address) => Self::V4(address),
            IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
                || Self::V6Prefix(Ipv6Addr::from(u128::from(address) & !u128::from(u64::MAX))),
                Self::V4,
            ),
        }
    }
}

/// `limits.publishers_per_address`. Cloning shares one count.
#[derive(Clone, Debug)]
pub struct PublishersPerAddress {
    maximum: NonZeroUsize,
    held: Arc<Mutex<HashMap<AddressKey, usize>>>,
}

/// One address already holds its maximum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{address} already holds {maximum} publishers (limits.publishers_per_address)")]
pub struct AddressFull {
    pub address: AddressKey,
    pub maximum: usize,
}

impl PublishersPerAddress {
    pub fn new(maximum: NonZeroUsize) -> Self {
        Self {
            maximum,
            held: Arc::default(),
        }
    }

    /// Counts one more publisher for `address`, or refuses without waiting:
    /// a client at its limit is told so rather than queued behind itself.
    pub fn try_acquire(&self, address: IpAddr) -> Result<AddressPermit, AddressFull> {
        let key = AddressKey::from(address);
        let mut held = self.held.lock();
        let count = held.entry(key).or_default();
        if *count >= self.maximum.get() {
            return Err(AddressFull {
                address: key,
                maximum: self.maximum.get(),
            });
        }
        *count += 1;
        Ok(AddressPermit {
            key,
            held: Arc::clone(&self.held),
        })
    }

    #[cfg(test)]
    fn held(&self, address: IpAddr) -> usize {
        self.held
            .lock()
            .get(&AddressKey::from(address))
            .copied()
            .unwrap_or(0)
    }
}

/// One counted publisher, returned when dropped.
#[derive(Debug)]
pub struct AddressPermit {
    key: AddressKey,
    held: Arc<Mutex<HashMap<AddressKey, usize>>>,
}

impl Drop for AddressPermit {
    fn drop(&mut self) {
        let mut held = self.held.lock();
        if let Some(count) = held.get_mut(&self.key) {
            *count -= 1;
            // Emptied entries are removed so the map tracks connected
            // clients, not every address that ever connected.
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Result = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn an_address_is_refused_at_its_maximum_until_a_permit_returns() -> Result {
        let limit = PublishersPerAddress::new(nz::usize!(2));
        let client: IpAddr = "203.0.113.7".parse()?;
        let first = limit.try_acquire(client)?;
        let _second = limit.try_acquire(client)?;
        assert_eq!(
            limit.try_acquire(client).map(|_| ()),
            Err(AddressFull {
                address: AddressKey::from(client),
                maximum: 2
            })
        );
        // Another client is unaffected.
        let _other = limit.try_acquire("203.0.113.8".parse()?)?;
        drop(first);
        let _third = limit.try_acquire(client)?;
        Ok(())
    }

    #[test]
    fn ipv6_counts_per_64_and_mapped_ipv4_counts_as_ipv4() -> Result {
        let limit = PublishersPerAddress::new(nz::usize!(1));
        let _held = limit.try_acquire("2001:db8:1:2::7".parse()?)?;
        assert!(limit.try_acquire("2001:db8:1:2:ffff::1".parse()?).is_err());
        let _neighbour = limit.try_acquire("2001:db8:1:3::7".parse()?)?;

        let _mapped = limit.try_acquire("::ffff:203.0.113.7".parse()?)?;
        assert!(limit.try_acquire("203.0.113.7".parse()?).is_err());
        assert_eq!(
            AddressKey::from("2001:db8:1:2::7".parse::<IpAddr>()?).to_string(),
            "2001:db8:1:2::/64"
        );
        Ok(())
    }

    #[test]
    fn released_addresses_leave_no_entry() -> Result {
        let limit = PublishersPerAddress::new(nz::usize!(4));
        let client: IpAddr = "198.51.100.1".parse()?;
        drop(limit.try_acquire(client)?);
        assert_eq!(limit.held(client), 0);
        assert!(limit.held.lock().is_empty());
        Ok(())
    }
}

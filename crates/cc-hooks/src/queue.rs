//! What is waiting to be delivered to one endpoint.
//!
//! # Order
//!
//! Events for one subject are delivered in the order they occurred, because the
//! order is the meaning: an "ended" arriving before the "available" it followed
//! would tell an automation system to tear down something still running. Only
//! one request per subject is therefore in flight at a time.
//!
//! Different subjects are unrelated and go concurrently, so one slow subject
//! cannot hold up the rest. Nothing is promised about their relative order.
//!
//! # Overflow
//!
//! Full means the endpoint has been failing for a long time. The **oldest**
//! queued event is dropped, never the newest: what survives is then a suffix of
//! each subject's history, so surviving events stay in order and the most
//! recent state — which is the one worth having — is the state that is kept.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{Envelope, Kind, Subject};

/// Pending work for one hook, ordered per subject.
#[derive(Debug)]
pub struct Queue<S: Subject, K: Kind> {
    waiting: HashMap<S, VecDeque<Queued<S, K>>>,
    /// Subjects with work and nothing in flight, in the order they became ready.
    ready: VecDeque<S>,
    /// Subjects with a request in flight. Their queues are held back.
    sending: HashSet<S>,
    capacity: usize,
    queued: usize,
    next_sequence: u64,
}

#[derive(Debug)]
struct Queued<S: Subject, K: Kind> {
    /// Arrival order across every subject, so the oldest is findable on overflow.
    sequence: u64,
    envelope: Envelope<S, K>,
}

impl<S: Subject, K: Kind> Queue<S, K> {
    pub fn new(capacity: usize) -> Self {
        Self {
            waiting: HashMap::new(),
            ready: VecDeque::new(),
            sending: HashSet::new(),
            capacity,
            queued: 0,
            next_sequence: 0,
        }
    }

    /// Accepts an event, evicting the oldest if that puts it over capacity.
    ///
    /// Returns whether something had to be dropped, which the caller counts.
    pub fn push(&mut self, envelope: Envelope<S, K>) -> bool {
        let subject = envelope.subject.clone();
        let sequence = self.next_sequence;
        self.next_sequence += 1;

        let queue = self.waiting.entry(subject.clone()).or_default();
        let was_idle = queue.is_empty();
        queue.push_back(Queued { sequence, envelope });
        self.queued += 1;
        // A subject already in flight is not ready: its next event waits for the
        // one ahead of it to finish, which is what keeps the order.
        if was_idle && !self.sending.contains(&subject) {
            self.ready.push_back(subject);
        }

        self.queued > self.capacity && self.evict_oldest()
    }

    /// Takes the next event for a subject that has nothing in flight.
    pub fn take_ready(&mut self) -> Option<Envelope<S, K>> {
        let subject = self.ready.pop_front()?;
        let queue = self.waiting.get_mut(&subject)?;
        let queued = queue.pop_front()?;
        self.queued -= 1;
        if queue.is_empty() {
            self.waiting.remove(&subject);
        }
        self.sending.insert(subject);
        Some(queued.envelope)
    }

    /// Releases a subject once its request has finished, retries included.
    pub fn finish(&mut self, subject: &S) {
        self.sending.remove(subject);
        if self.waiting.contains_key(subject) {
            self.ready.push_back(subject.clone());
        }
    }

    /// Waiting events, excluding the one event each active subject may have in
    /// flight. The dispatcher mirrors this into an atomic metrics gauge.
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// Abandons everything still waiting, reporting how much was lost.
    pub fn clear(&mut self) -> usize {
        let lost = self.queued;
        self.waiting.clear();
        self.ready.clear();
        self.queued = 0;
        lost
    }

    /// Drops the event that has waited longest, across every subject.
    ///
    /// A scan, because it only happens once the queue is already full — paying
    /// for an index on every push to speed up the pathological path would be the
    /// wrong trade.
    fn evict_oldest(&mut self) -> bool {
        let Some(subject) = self
            .waiting
            .iter()
            .filter_map(|(subject, queue)| Some((queue.front()?.sequence, subject)))
            .min_by_key(|(sequence, _)| *sequence)
            .map(|(_, subject)| subject.clone())
        else {
            return false;
        };

        let queue = self.waiting.entry(subject.clone()).or_default();
        queue.pop_front();
        self.queued -= 1;
        if queue.is_empty() {
            self.waiting.remove(&subject);
            self.ready.retain(|ready| ready != &subject);
        }
        true
    }
}

//! HTTP accounting follows handler and body lifetimes, not resolved object size.
use super::DurationHistogram;
use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HttpResource {
    Playlist,
    Media,
    Operator,
    Other,
}
impl HttpResource {
    pub const ALL: [Self; 4] = [Self::Playlist, Self::Media, Self::Operator, Self::Other];
    pub const fn name(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Media => "media",
            Self::Operator => "operator",
            Self::Other => "other",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HttpMethod {
    Get,
    Head,
    Other,
}
impl HttpMethod {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Other => "other",
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct HttpClassSnapshot {
    pub started: u64,
    pub in_flight: usize,
    pub body_bytes: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub errors: u64,
    pub handler_duration: DurationHistogram,
    pub body_duration: DurationHistogram,
}
#[derive(Clone, Debug, Default)]
pub struct HttpSnapshot {
    pub classes: [HttpClassSnapshot; 4],
    /// Only enum keys and valid HTTP status codes enter this bounded map.
    pub responses: BTreeMap<(HttpResource, HttpMethod, u16), u64>,
    pub requests_rejected: u64,
    pub connections_rejected: u64,
    pub connections: usize,
    pub request_capacity: usize,
    pub connection_capacity: usize,
}
#[derive(Clone, Debug, Default)]
pub struct HttpMeters(pub Arc<Mutex<HttpSnapshot>>);
impl HttpMeters {
    pub fn snapshot(&self) -> HttpSnapshot {
        self.0.lock().clone()
    }
    pub fn start(&self, resource: HttpResource, method: HttpMethod) -> HttpObservation {
        let mut state = self.0.lock();
        state.classes[resource as usize].started += 1;
        state.classes[resource as usize].in_flight += 1;
        HttpObservation {
            meters: self.clone(),
            resource,
            method,
            started: Instant::now(),
            body_started: None,
            finished: false,
        }
    }
}

pub struct HttpObservation {
    meters: HttpMeters,
    resource: HttpResource,
    method: HttpMethod,
    started: Instant,
    body_started: Option<Instant>,
    finished: bool,
}
impl HttpObservation {
    pub fn response(&mut self, status: u16) {
        let mut state = self.meters.0.lock();
        *state
            .responses
            .entry((self.resource, self.method, status))
            .or_default() += 1;
        state.classes[self.resource as usize]
            .handler_duration
            .observe(self.started.elapsed());
        self.body_started = Some(Instant::now());
    }
    pub fn bytes(&self, bytes: usize) {
        self.meters.0.lock().classes[self.resource as usize].body_bytes += bytes as u64;
    }
    pub fn finish(&mut self, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut state = self.meters.0.lock();
        let class = &mut state.classes[self.resource as usize];
        class.in_flight -= 1;
        match outcome {
            "completed" => class.completed += 1,
            "error" => class.errors += 1,
            _ => class.cancelled += 1,
        }
        if let Some(started) = self.body_started {
            class.body_duration.observe(started.elapsed());
        } else {
            class.handler_duration.observe(self.started.elapsed());
        }
    }
}
impl Drop for HttpObservation {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
}

use std::sync::Arc;

use parking_lot::{Condvar, Mutex};
use tokio::sync::{mpsc, oneshot};

use crate::source::{DiscoveryLimits, DiscoveryReport, InputLimits, Packet, SourceError};

use super::{
    AvformatConfig,
    control::Control,
    ffi::{AvPacket, FormatInput, ReadError},
    input::AvformatInput,
};

pub enum WorkerEvent {
    Packet(QueuedPacket),
    End(crate::source::InputState),
    Error(SourceError),
}

pub struct QueuedPacket {
    packet: Packet,
    _permit: PayloadPermit,
}

impl QueuedPacket {
    pub fn payload_len(&self) -> usize {
        self.packet.payload.len()
    }

    pub fn into_packet(self) -> Packet {
        self.packet
    }
}

struct PayloadBudget {
    maximum: usize,
    used: Mutex<usize>,
    released: Condvar,
}

impl PayloadBudget {
    fn new(maximum: usize) -> Arc<Self> {
        Arc::new(Self {
            maximum,
            used: Mutex::new(0),
            released: Condvar::new(),
        })
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> PayloadPermit {
        let mut used = self.used.lock();
        while used.saturating_add(bytes) > self.maximum {
            self.released.wait(&mut used);
        }
        *used += bytes;
        PayloadPermit {
            budget: Arc::clone(self),
            bytes,
        }
    }
}

struct PayloadPermit {
    budget: Arc<PayloadBudget>,
    bytes: usize,
}

impl Drop for PayloadPermit {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock();
        *used -= self.bytes;
        self.budget.released.notify_one();
    }
}

pub fn spawn(
    input: Box<dyn AvformatInput>,
    config: AvformatConfig,
    input_limits: InputLimits,
    discovery_limits: DiscoveryLimits,
    control: Arc<Control>,
    discovery: oneshot::Sender<Result<DiscoveryReport, SourceError>>,
    output: mpsc::Sender<WorkerEvent>,
) -> Result<(), SourceError> {
    std::thread::Builder::new()
        .name("rushls-avformat".into())
        .spawn(move || {
            run(
                input,
                config,
                input_limits,
                discovery_limits,
                control,
                discovery,
                output,
            );
        })
        .map(|_| ())
        .map_err(|error| SourceError::Open(format!("could not start AVFormat worker: {error}")))
}

fn run(
    input: Box<dyn AvformatInput>,
    config: AvformatConfig,
    input_limits: InputLimits,
    discovery_limits: DiscoveryLimits,
    control: Arc<Control>,
    discovery: oneshot::Sender<Result<DiscoveryReport, SourceError>>,
    output: mpsc::Sender<WorkerEvent>,
) {
    let (mut format, catalog) = match FormatInput::open(
        input,
        Arc::clone(&control),
        discovery_limits,
        config.io_buffer_size.get(),
    ) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = discovery.send(Err(error));
            return;
        }
    };
    if discovery.send(Ok(catalog.report().clone())).is_err() {
        return;
    }

    let mut packet = match AvPacket::new() {
        Ok(packet) => packet,
        Err(error) => {
            let _ = output.blocking_send(WorkerEvent::Error(error));
            return;
        }
    };
    let budget = PayloadBudget::new(config.maximum_queued_payload_bytes.get());
    loop {
        let result = format.read(&mut packet);
        if result < 0 {
            match format.read_error(result) {
                ReadError::End(state) => {
                    let _ = output.blocking_send(WorkerEvent::End(state));
                }
                ReadError::Failed(error) => {
                    let _ = output.blocking_send(WorkerEvent::Error(error));
                }
                ReadError::Cancelled => {}
            }
            return;
        }

        // Packet-owned memory is valid only until `unref`, so conversion and
        // parameter validation happen together before releasing it.
        let converted = (|| {
            // SAFETY: the packet came from this open format context.
            let track_id = unsafe { catalog.validate_packet(format.context(), packet.as_ptr()) }?;
            track_id
                .map(|track_id| {
                    packet.to_packet(track_id, input_limits.maximum_payload_bytes_per_packet)
                })
                .transpose()
        })();
        packet.unref();

        match converted {
            Ok(Some(packet)) => {
                let permit = budget.reserve(packet.payload.len());
                if output
                    .blocking_send(WorkerEvent::Packet(QueuedPacket {
                        packet,
                        _permit: permit,
                    }))
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => {}
            Err(error) => {
                let _ = output.blocking_send(WorkerEvent::Error(error));
                return;
            }
        }
    }
}

use super::PublishMode;
use std::sync::Arc;

pub enum OutstandingRequest {
    ConnectionRequest {
        app_name: Arc<str>,
        transaction_id: f64,
    },

    PublishRequested {
        stream_key: Arc<str>,
        mode: PublishMode,
        stream_id: u32,
    },

    PlayRequested {
        stream_key: Arc<str>,
        stream_id: u32,
    },
}

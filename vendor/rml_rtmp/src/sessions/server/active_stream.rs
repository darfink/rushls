use super::PublishMode;
use std::sync::Arc;

pub enum StreamState {
    Created,

    Publishing {
        stream_key: Arc<str>,
        #[allow(dead_code)]
        mode: PublishMode,
    },

    Playing {
        stream_key: Arc<str>,
    },

    Completed,
}

pub struct ActiveStream {
    pub current_state: StreamState,
}

//! Lazy worker for durable Fleet Attention acknowledgements.

use std::collections::HashSet;
use std::sync::mpsc::TryRecvError;

use crate::fleet_acknowledgements::AttentionAcknowledgementStore;
use crate::tui::worker::Worker;

enum Request {
    Load,
    Acknowledge(String),
}

pub(crate) enum AttentionAckResult {
    Loaded(Result<HashSet<String>, String>),
    Acknowledged {
        item_id: String,
        result: Result<(), String>,
    },
}

pub(crate) struct AttentionAckPoller {
    worker: Worker<Request, AttentionAckResult>,
}

impl AttentionAckPoller {
    pub(crate) fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-attention-ack-worker", handle),
        }
    }

    pub(crate) fn request_load(&self) {
        self.worker.request(Request::Load);
    }

    pub(crate) fn request_acknowledge(&self, item_id: String) {
        self.worker.request(Request::Acknowledge(item_id));
    }

    pub(crate) fn try_recv(&self) -> Result<AttentionAckResult, TryRecvError> {
        self.worker.try_recv()
    }
}

fn handle(request: Request) -> AttentionAckResult {
    match request {
        Request::Load => AttentionAckResult::Loaded(
            AttentionAcknowledgementStore::open()
                .and_then(|store| store.load())
                .map_err(|error| error.to_string()),
        ),
        Request::Acknowledge(item_id) => {
            let result = AttentionAcknowledgementStore::open()
                .and_then(|store| store.acknowledge(&item_id))
                .map_err(|error| error.to_string());
            AttentionAckResult::Acknowledged { item_id, result }
        }
    }
}

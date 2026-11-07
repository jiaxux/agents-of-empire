//! Lazy background worker for durable campaign metadata.

use std::sync::mpsc::TryRecvError;

use crate::campaigns::{
    CampaignLoadReport, CampaignManifest, CampaignStore, CampaignTaskCreateRequest,
    CampaignTaskState,
};
use crate::tui::worker::Worker;

enum CampaignRequest {
    Load,
    AddTask {
        campaign_id: Option<String>,
        request: CampaignTaskCreateRequest,
    },
    SetTaskState {
        campaign_id: String,
        task_id: String,
        state: CampaignTaskState,
    },
}

pub(crate) enum CampaignWorkerResult {
    Loaded(Result<CampaignLoadReport, String>),
    Updated {
        campaign_id: Option<String>,
        result: Result<CampaignManifest, String>,
    },
}

pub(crate) struct CampaignPoller {
    worker: Worker<CampaignRequest, CampaignWorkerResult>,
}

impl CampaignPoller {
    pub(crate) fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-campaign-worker", handle),
        }
    }

    pub(crate) fn request_load(&self) {
        self.worker.request(CampaignRequest::Load);
    }

    pub(crate) fn request_add_task(
        &self,
        campaign_id: Option<String>,
        request: CampaignTaskCreateRequest,
    ) {
        self.worker.request(CampaignRequest::AddTask {
            campaign_id,
            request,
        });
    }

    pub(crate) fn request_set_task_state(
        &self,
        campaign_id: String,
        task_id: String,
        state: CampaignTaskState,
    ) {
        self.worker.request(CampaignRequest::SetTaskState {
            campaign_id,
            task_id,
            state,
        });
    }

    pub(crate) fn try_recv(&self) -> Result<CampaignWorkerResult, TryRecvError> {
        self.worker.try_recv()
    }
}

fn handle(request: CampaignRequest) -> CampaignWorkerResult {
    match request {
        CampaignRequest::Load => CampaignWorkerResult::Loaded(
            CampaignStore::open()
                .and_then(|store| store.load())
                .map_err(|error| error.to_string()),
        ),
        CampaignRequest::AddTask {
            campaign_id,
            request,
        } => {
            let result = CampaignStore::open()
                .and_then(|store| {
                    let campaign = match campaign_id.as_deref() {
                        Some(id) => store.inspect(id)?,
                        None => store.ensure_default()?,
                    };
                    store.add_task(&campaign.id, request)
                })
                .map_err(|error| error.to_string());
            CampaignWorkerResult::Updated {
                campaign_id,
                result,
            }
        }
        CampaignRequest::SetTaskState {
            campaign_id,
            task_id,
            state,
        } => {
            let result = CampaignStore::open()
                .and_then(|store| store.set_task_state(&campaign_id, &task_id, state))
                .map_err(|error| error.to_string());
            CampaignWorkerResult::Updated {
                campaign_id: Some(campaign_id),
                result,
            }
        }
    }
}

//! Lazy background worker for durable Agent Trial metadata.

use std::sync::mpsc::TryRecvError;

use crate::trials::{TrialCreateRequest, TrialLoadReport, TrialManifest, TrialStore, TrialUpdate};
use crate::tui::worker::Worker;

enum TrialRequest {
    Load,
    Create(TrialCreateRequest),
    Update {
        trial_id: String,
        contender_id: String,
        update: TrialUpdate,
    },
    CompleteAndVerify {
        trial_id: String,
        contender_id: String,
        elapsed_ms: u64,
        worktree: std::path::PathBuf,
    },
    Retry {
        trial_id: String,
        contender_id: String,
    },
    Cancel(String),
    Retain {
        trial_id: String,
        contender_id: String,
    },
}

pub(crate) enum TrialWorkerResult {
    Loaded(Result<TrialLoadReport, String>),
    Created {
        trial_id: String,
        result: Result<TrialManifest, String>,
    },
    Updated {
        trial_id: String,
        contender_id: Option<String>,
        result: Result<TrialManifest, String>,
    },
    Retried {
        trial_id: String,
        contender_id: String,
        result: Result<TrialManifest, String>,
    },
}

pub(crate) struct TrialPoller {
    worker: Worker<TrialRequest, TrialWorkerResult>,
}

impl TrialPoller {
    pub(crate) fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-trial-worker", handle),
        }
    }

    pub(crate) fn request_load(&self) {
        self.worker.request(TrialRequest::Load);
    }

    pub(crate) fn request_create(&self, request: TrialCreateRequest) {
        self.worker.request(TrialRequest::Create(request));
    }

    pub(crate) fn request_update(
        &self,
        trial_id: String,
        contender_id: String,
        update: TrialUpdate,
    ) {
        self.worker.request(TrialRequest::Update {
            trial_id,
            contender_id,
            update,
        });
    }

    pub(crate) fn request_complete_and_verify(
        &self,
        trial_id: String,
        contender_id: String,
        elapsed_ms: u64,
        worktree: std::path::PathBuf,
    ) {
        self.worker.request(TrialRequest::CompleteAndVerify {
            trial_id,
            contender_id,
            elapsed_ms,
            worktree,
        });
    }

    pub(crate) fn request_retry(&self, trial_id: String, contender_id: String) {
        self.worker.request(TrialRequest::Retry {
            trial_id,
            contender_id,
        });
    }

    pub(crate) fn request_cancel(&self, trial_id: String) {
        self.worker.request(TrialRequest::Cancel(trial_id));
    }

    pub(crate) fn request_retain(&self, trial_id: String, contender_id: String) {
        self.worker.request(TrialRequest::Retain {
            trial_id,
            contender_id,
        });
    }

    pub(crate) fn try_recv(&self) -> Result<TrialWorkerResult, TryRecvError> {
        self.worker.try_recv()
    }
}

fn handle(request: TrialRequest) -> TrialWorkerResult {
    match request {
        TrialRequest::Load => TrialWorkerResult::Loaded(
            TrialStore::open()
                .and_then(|store| store.load())
                .map_err(|error| error.to_string()),
        ),
        TrialRequest::Create(request) => {
            let trial_id = request.trial_id().to_string();
            let result = TrialStore::open()
                .and_then(|store| store.create(request))
                .map_err(|error| error.to_string());
            TrialWorkerResult::Created { trial_id, result }
        }
        TrialRequest::Update {
            trial_id,
            contender_id,
            update,
        } => {
            let result = TrialStore::open()
                .and_then(|store| store.update(&trial_id, &contender_id, update))
                .map_err(|error| error.to_string());
            TrialWorkerResult::Updated {
                trial_id,
                contender_id: Some(contender_id),
                result,
            }
        }
        TrialRequest::CompleteAndVerify {
            trial_id,
            contender_id,
            elapsed_ms,
            worktree,
        } => {
            let result = TrialStore::open()
                .and_then(|store| {
                    let completed = store.update(
                        &trial_id,
                        &contender_id,
                        TrialUpdate::Completed { elapsed_ms },
                    )?;
                    let Some(recipe) = completed.verification_recipe.as_ref() else {
                        return Ok(completed);
                    };
                    let execution = crate::trials::run_verification(&worktree, recipe);
                    store.update(
                        &trial_id,
                        &contender_id,
                        TrialUpdate::Verified {
                            result: execution.result,
                            changed_files: execution.changed_files,
                            insertions: execution.insertions,
                            deletions: execution.deletions,
                        },
                    )
                })
                .map_err(|error| error.to_string());
            TrialWorkerResult::Updated {
                trial_id,
                contender_id: Some(contender_id),
                result,
            }
        }
        TrialRequest::Retry {
            trial_id,
            contender_id,
        } => {
            let result = TrialStore::open()
                .and_then(|store| store.retry(&trial_id, &contender_id))
                .map_err(|error| error.to_string());
            TrialWorkerResult::Retried {
                trial_id,
                contender_id,
                result,
            }
        }
        TrialRequest::Cancel(trial_id) => {
            let result = TrialStore::open()
                .and_then(|store| store.cancel(&trial_id))
                .map_err(|error| error.to_string());
            TrialWorkerResult::Updated {
                trial_id,
                contender_id: None,
                result,
            }
        }
        TrialRequest::Retain {
            trial_id,
            contender_id,
        } => {
            let result = TrialStore::open()
                .and_then(|store| store.retain_winner(&trial_id, &contender_id))
                .map_err(|error| error.to_string());
            TrialWorkerResult::Updated {
                trial_id,
                contender_id: Some(contender_id),
                result,
            }
        }
    }
}

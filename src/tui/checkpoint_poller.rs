//! Lazy background worker for durable checkpoint history and capture.

use std::sync::mpsc::TryRecvError;

use crate::checkpoints::{
    CaptureRequest, CheckpointDeleteResult, CheckpointExportResult, CheckpointForkPlan,
    CheckpointManifest, CheckpointRestorePlan, CheckpointRestoreResult, CheckpointStore,
    CheckpointSummary, LoadReport,
};
use crate::tui::worker::Worker;

enum CheckpointRequest {
    Load,
    Capture(Box<CaptureRequest>),
    PrepareFork {
        checkpoint_id: String,
        source_session_id: String,
        source_repo_path: std::path::PathBuf,
        retry: bool,
    },
    Inspect(String),
    Export(String),
    Delete(String),
    PrepareRestore {
        checkpoint_id: String,
        target: std::path::PathBuf,
    },
    Restore(Box<CheckpointRestorePlan>),
}

pub(crate) enum CheckpointWorkerResult {
    Loaded(Result<LoadReport, String>),
    Captured {
        checkpoint_id: String,
        result: Result<CheckpointSummary, String>,
    },
    ForkPrepared {
        checkpoint_id: String,
        result: Result<CheckpointForkPlan, String>,
    },
    Inspected {
        checkpoint_id: String,
        result: Result<CheckpointManifest, String>,
    },
    Exported {
        checkpoint_id: String,
        result: Result<CheckpointExportResult, String>,
    },
    Deleted {
        checkpoint_id: String,
        result: Result<CheckpointDeleteResult, String>,
    },
    RestorePrepared {
        checkpoint_id: String,
        result: Result<CheckpointRestorePlan, String>,
    },
    Restored {
        checkpoint_id: String,
        result: Result<CheckpointRestoreResult, String>,
    },
}

pub(crate) struct CheckpointPoller {
    worker: Worker<CheckpointRequest, CheckpointWorkerResult>,
}

impl CheckpointPoller {
    pub(crate) fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-checkpoint-worker", handle),
        }
    }

    pub(crate) fn request_load(&self) {
        self.worker.request(CheckpointRequest::Load);
    }

    pub(crate) fn request_capture(&self, request: CaptureRequest) {
        self.worker
            .request(CheckpointRequest::Capture(Box::new(request)));
    }

    pub(crate) fn request_fork_preparation(
        &self,
        checkpoint_id: String,
        source_session_id: String,
        source_repo_path: std::path::PathBuf,
    ) {
        self.worker.request(CheckpointRequest::PrepareFork {
            checkpoint_id,
            source_session_id,
            source_repo_path,
            retry: false,
        });
    }

    pub(crate) fn request_retry_fork_preparation(
        &self,
        checkpoint_id: String,
        template_session_id: String,
        template_repo_path: std::path::PathBuf,
    ) {
        self.worker.request(CheckpointRequest::PrepareFork {
            checkpoint_id,
            source_session_id: template_session_id,
            source_repo_path: template_repo_path,
            retry: true,
        });
    }

    pub(crate) fn request_inspect(&self, checkpoint_id: String) {
        self.worker
            .request(CheckpointRequest::Inspect(checkpoint_id));
    }

    pub(crate) fn request_export(&self, checkpoint_id: String) {
        self.worker
            .request(CheckpointRequest::Export(checkpoint_id));
    }

    pub(crate) fn request_delete(&self, checkpoint_id: String) {
        self.worker
            .request(CheckpointRequest::Delete(checkpoint_id));
    }

    pub(crate) fn request_restore_preparation(
        &self,
        checkpoint_id: String,
        target: std::path::PathBuf,
    ) {
        self.worker.request(CheckpointRequest::PrepareRestore {
            checkpoint_id,
            target,
        });
    }

    pub(crate) fn request_restore(&self, plan: CheckpointRestorePlan) {
        self.worker
            .request(CheckpointRequest::Restore(Box::new(plan)));
    }

    pub(crate) fn try_recv(&self) -> Result<CheckpointWorkerResult, TryRecvError> {
        self.worker.try_recv()
    }
}

fn handle(request: CheckpointRequest) -> CheckpointWorkerResult {
    match request {
        CheckpointRequest::Load => CheckpointWorkerResult::Loaded(
            CheckpointStore::open()
                .and_then(|store| store.load())
                .map_err(|error| error.to_string()),
        ),
        CheckpointRequest::Capture(request) => {
            let checkpoint_id = request.checkpoint_id().to_string();
            let result = CheckpointStore::open()
                .and_then(|store| store.capture(*request))
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::Captured {
                checkpoint_id,
                result,
            }
        }
        CheckpointRequest::PrepareFork {
            checkpoint_id,
            source_session_id,
            source_repo_path,
            retry,
        } => {
            let result = CheckpointStore::open()
                .and_then(|store| {
                    if retry {
                        store.prepare_retry_fork(
                            &checkpoint_id,
                            &source_session_id,
                            &source_repo_path,
                        )
                    } else {
                        store.prepare_fork(&checkpoint_id, &source_session_id, &source_repo_path)
                    }
                })
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::ForkPrepared {
                checkpoint_id,
                result,
            }
        }
        CheckpointRequest::Inspect(checkpoint_id) => {
            let result = CheckpointStore::open()
                .and_then(|store| store.inspect(&checkpoint_id))
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::Inspected {
                checkpoint_id,
                result,
            }
        }
        CheckpointRequest::Export(checkpoint_id) => {
            let result = CheckpointStore::open()
                .and_then(|store| store.export_default(&checkpoint_id))
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::Exported {
                checkpoint_id,
                result,
            }
        }
        CheckpointRequest::Delete(checkpoint_id) => {
            let result = CheckpointStore::open()
                .and_then(|store| store.delete(&checkpoint_id))
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::Deleted {
                checkpoint_id,
                result,
            }
        }
        CheckpointRequest::PrepareRestore {
            checkpoint_id,
            target,
        } => {
            let result = CheckpointStore::open()
                .and_then(|store| store.prepare_restore(&checkpoint_id, target))
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::RestorePrepared {
                checkpoint_id,
                result,
            }
        }
        CheckpointRequest::Restore(plan) => {
            let checkpoint_id = plan.seed.checkpoint_id.clone();
            let result = CheckpointStore::open()
                .and_then(|store| store.restore(&plan))
                .map_err(|error| error.to_string());
            CheckpointWorkerResult::Restored {
                checkpoint_id,
                result,
            }
        }
    }
}

//! Bounded background producer for the conflict radar.
//!
//! Resolving a base ref and listing a worktree's changed files are git
//! operations, so they run on a worker thread. The UI thread only enqueues
//! [`ScanRequest`]s and drains [`ScanOutcome`]s; it never waits on one.

use std::sync::mpsc::TryRecvError;

use crate::tui::fleet_state::{ScanOutcome, ScanRequest};
use crate::tui::worker::Worker;

pub(crate) struct FleetPoller {
    worker: Worker<ScanRequest, ScanOutcome>,
}

impl FleetPoller {
    pub(crate) fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-fleet-worker", scan_now),
        }
    }

    pub(crate) fn request(&self, request: ScanRequest) {
        self.worker.request(request);
    }

    pub(crate) fn try_recv(&self) -> Result<ScanOutcome, TryRecvError> {
        self.worker.try_recv()
    }
}

pub(crate) fn scan_now(request: ScanRequest) -> ScanOutcome {
    let session_id = request.session_id.clone();
    let generation = request.generation;
    ScanOutcome {
        session_id,
        generation,
        result: changed_paths(&request),
    }
}

fn changed_paths(request: &ScanRequest) -> Result<Vec<String>, String> {
    let repo = std::path::Path::new(&request.scan_path);
    if !crate::git::GitWorktree::is_git_repo(repo) {
        return Err("not a git repository".to_string());
    }
    let base = request
        .base_branch
        .as_deref()
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .map(str::to_string)
        .or_else(|| crate::git::diff::get_default_base_ref(repo).ok())
        .ok_or_else(|| "no base branch to compare against".to_string())?;
    crate::git::diff::compute_changed_files(repo, &base)
        .map(|files| {
            files
                .into_iter()
                .map(|file| file.path.to_string_lossy().into_owned())
                .collect()
        })
        .map_err(|error| error.to_string())
}

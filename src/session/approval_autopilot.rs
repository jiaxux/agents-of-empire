//! External, opt-in permission responder for terminal agent sessions.
//!
//! The status pollers call this only after their normal pane observation says
//! a session is waiting. A second, deliberately strict detector verifies the
//! pane is an actual tool-permission UI before this module sends the safest
//! approval advertised by that exact menu. Cross-process locking and a short durable marker
//! keep a TUI and `aoe serve` from answering the same prompt twice.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Instance, Status};

const STATE_DIR: &str = "approval-autopilot";
const PROMPT_FINGERPRINT_VERSION: &str = "v2-confirm-selection";
const SAME_PROMPT_RETRY_MS: i64 = 60_000;
const MAX_MARKER_BYTES: u64 = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalOutcome {
    Approved,
    Ignored,
    AlreadyHandled,
    Busy,
}

enum ApprovalKeys {
    Mapped(&'static [crate::agents::KeyToken]),
    PromptSpecific([crate::agents::KeyToken; 2]),
}

impl ApprovalKeys {
    fn as_slice(&self) -> &[crate::agents::KeyToken] {
        match self {
            Self::Mapped(keys) => keys,
            Self::PromptSpecific(keys) => keys,
        }
    }
}

fn approval_keys(
    response: crate::agents::PermissionResponse,
    approval: crate::tmux::PermissionPromptApproval,
) -> ApprovalKeys {
    match approval {
        crate::tmux::PermissionPromptApproval::AllowOnce => ApprovalKeys::Mapped(response.allow),
        crate::tmux::PermissionPromptApproval::MappedAllowAlways => {
            ApprovalKeys::Mapped(response.allow_always.unwrap_or(response.allow))
        }
        crate::tmux::PermissionPromptApproval::Literal(key) => ApprovalKeys::PromptSpecific([
            crate::agents::KeyToken::Literal(key),
            crate::agents::KeyToken::Named("Enter"),
        ]),
    }
}

/// Verify and answer one pending permission prompt, then optimistically keep
/// the row Running until the next normal status observation confirms it. This
/// avoids a transient Waiting notification for a prompt AoE already handled.
pub(crate) fn maybe_approve_and_mark_running(
    instance: &mut Instance,
    tmux_session_name: &str,
    observed_pane: Option<&str>,
) {
    if maybe_approve(instance, tmux_session_name, observed_pane) != ApprovalOutcome::Approved {
        return;
    }
    instance.status = Status::Running;
    instance.live_status_baseline = Some(Status::Running);
    instance.idle_entered_at = None;
    instance.last_error = None;
}

#[derive(Debug, Serialize, Deserialize)]
struct ApprovalMarker {
    fingerprint: String,
    approved_at_unix_ms: i64,
}

/// Best-effort Approval Autopilot entry point. Errors are logged without pane
/// content and leave the prompt untouched, which is the safe failure mode.
fn maybe_approve(
    instance: &Instance,
    tmux_session_name: &str,
    observed_pane: Option<&str>,
) -> ApprovalOutcome {
    match try_maybe_approve(instance, tmux_session_name, observed_pane) {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                target: "session.approval_autopilot",
                session_id = %instance.id,
                %error,
                "approval autopilot left prompt untouched after responder error"
            );
            ApprovalOutcome::Ignored
        }
    }
}

fn try_maybe_approve(
    instance: &Instance,
    tmux_session_name: &str,
    observed_pane: Option<&str>,
) -> Result<ApprovalOutcome> {
    if instance.status != Status::Waiting || instance.is_structured() {
        return Ok(ApprovalOutcome::Ignored);
    }

    // A built-in tool owns its own terminal UI. A custom tool may explicitly
    // inherit a built-in detector through `agent_detect_as`; in that case the
    // inherited agent also supplies the response mapping.
    let Some(agent) = crate::agents::get_agent(&instance.tool).or_else(|| {
        (!instance.detect_as.is_empty())
            .then(|| crate::agents::get_agent(&instance.detect_as))
            .flatten()
    }) else {
        return Ok(ApprovalOutcome::Ignored);
    };
    let Some(response) = agent.permission_response else {
        return Ok(ApprovalOutcome::Ignored);
    };

    let session = crate::tmux::Session::from_name(tmux_session_name);
    let captured;
    let pane = match observed_pane {
        Some(pane) => pane,
        None => {
            captured = session
                .capture_pane(50)
                .context("could not capture candidate permission prompt")?;
            &captured
        }
    };
    let Some(prompt) = crate::tmux::verified_permission_prompt_signature(pane, agent.name) else {
        return Ok(ApprovalOutcome::Ignored);
    };

    let fingerprint = fingerprint(&format!(
        "{}\n{}\n{}",
        PROMPT_FINGERPRINT_VERSION, agent.name, prompt.signature
    ));
    let path = marker_path(&instance.id)?;
    let mut marker_file = open_private_marker(&path)?;
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&marker_file) {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(ApprovalOutcome::Busy);
        }
        return Err(error).context("could not lock approval marker");
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    if read_marker(&mut marker_file)
        .as_ref()
        .is_some_and(|marker| should_suppress(marker, &fingerprint, now_ms))
    {
        return Ok(ApprovalOutcome::AlreadyHandled);
    }

    // Keep the cross-process lock held while sending. The durable marker is
    // written only after tmux accepts the whole sequence, so a crash or send
    // error cannot strand a prompt behind a false "already handled" record.
    let keys = approval_keys(response, prompt.approval);
    session
        .send_key_tokens(keys.as_slice())
        .context("could not send verified approval keys")?;
    write_marker(
        &mut marker_file,
        &ApprovalMarker {
            fingerprint,
            approved_at_unix_ms: now_ms,
        },
    )?;

    tracing::info!(
        target: "session.approval_autopilot",
        session_id = %instance.id,
        agent = agent.name,
        approval = ?prompt.approval,
        "approval autopilot allowed one verified tool request"
    );
    Ok(ApprovalOutcome::Approved)
}

fn marker_path(session_id: &str) -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?.join(STATE_DIR);
    create_private_dir(&dir)?;
    Ok(dir.join(format!("{}.json", fingerprint(session_id))))
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("could not create {}", path.display()))
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))
}

#[cfg(unix)]
fn open_private_marker(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

#[cfg(not(unix))]
fn open_private_marker(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

fn read_marker(file: &mut File) -> Option<ApprovalMarker> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut content = String::new();
    file.take(MAX_MARKER_BYTES)
        .read_to_string(&mut content)
        .ok()?;
    serde_json::from_str(&content).ok()
}

fn write_marker(file: &mut File, marker: &ApprovalMarker) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(&mut *file, marker)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn should_suppress(marker: &ApprovalMarker, fingerprint: &str, now_ms: i64) -> bool {
    let age_ms = now_ms.saturating_sub(marker.approved_at_unix_ms);
    age_ms < 0 || (marker.fingerprint == fingerprint && age_ms < SAME_PROMPT_RETRY_MS)
}

fn fingerprint(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(value.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{KeyToken, PermissionResponse};

    #[test]
    fn prompt_specific_persistent_key_overrides_generic_agent_mapping() {
        let response = PermissionResponse {
            allow: &[KeyToken::Literal("y")],
            allow_always: Some(&[KeyToken::Literal("p")]),
            deny: &[KeyToken::Literal("d")],
        };

        assert_eq!(
            approval_keys(
                response,
                crate::tmux::PermissionPromptApproval::Literal("a")
            )
            .as_slice(),
            &[KeyToken::Literal("a"), KeyToken::Named("Enter")]
        );
        assert_eq!(
            approval_keys(
                response,
                crate::tmux::PermissionPromptApproval::Literal("p")
            )
            .as_slice(),
            &[KeyToken::Literal("p"), KeyToken::Named("Enter")]
        );
        assert_eq!(
            approval_keys(response, crate::tmux::PermissionPromptApproval::AllowOnce).as_slice(),
            response.allow
        );
    }

    #[test]
    fn duplicate_prompt_is_suppressed_without_delaying_a_new_prompt() {
        let marker = ApprovalMarker {
            fingerprint: "same".to_string(),
            approved_at_unix_ms: 10_000,
        };

        assert!(should_suppress(&marker, "same", 14_999));
        assert!(!should_suppress(&marker, "different", 10_001));
        assert!(should_suppress(&marker, "same", 69_999));
        assert!(!should_suppress(&marker, "same", 70_000));
    }

    #[test]
    fn backward_wall_clock_is_safely_suppressed() {
        let marker = ApprovalMarker {
            fingerprint: "prompt".to_string(),
            approved_at_unix_ms: 20_000,
        };
        assert!(should_suppress(&marker, "new", 19_999));
    }
}

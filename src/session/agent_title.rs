//! Native agent chat title synchronization for a 1-to-1 name bijection between
//! AoE session titles and the underlying agent conversation stores (Claude
//! Code, Codex CLI, and Antigravity CLI).
//!
//! All writers are strictly best-effort and fail-closed: they only write when
//! the target agent's conversation store already exists on disk (never creating
//! ghost files or directories) and never block or fail an AoE rename.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::session::capture::{
    canonicalize_or_raw, claude_home_for_host_environment, encode_claude_project_path,
    is_valid_session_id, resolve_agent_home,
};
use crate::session::Instance;

/// Collapse newlines and control characters into single spaces and trim so a
/// title is safe for single-line CLI flags, slash commands, JSONL, and pbtxt.
pub(crate) fn sanitize_agent_title(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Shell-single-quote a value (`'foo'` with `'` escaped as `'\''`).
pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Return true when `cmd` already includes a Claude `-n` / `--name` option so
/// AoE never appends a duplicate flag over a user's explicit override.
pub(crate) fn command_has_name_flag(cmd: &str) -> bool {
    cmd.split_whitespace().any(|tok| {
        tok == "-n"
            || tok == "--name"
            || tok.starts_with("--name=")
            || (tok.starts_with("-n") && tok.len() > 2 && !tok.starts_with("--"))
    })
}

/// Synchronize `title` into the native conversation store of `instance`'s
/// agent, if `instance` has a captured `agent_session_id` and runs on the host.
pub(crate) fn sync_instance_agent_title(instance: &Instance, title: &str) {
    if instance.is_sandboxed() {
        return;
    }
    let Some(sid) = instance.agent_session_id.as_deref() else {
        return;
    };
    let host_env = instance.resolved_host_environment();
    sync_agent_session_title(
        &instance.tool,
        &instance.project_path,
        sid,
        title,
        &host_env,
    );
}

/// Synchronize `title` into the native on-disk title store for `tool` and
/// `session_id`. Best-effort: logs at `debug` on I/O failure and never fails
/// the caller.
pub(crate) fn sync_agent_session_title(
    tool: &str,
    project_path: &str,
    session_id: &str,
    title: &str,
    host_env: &[String],
) {
    let clean_title = sanitize_agent_title(title);
    if clean_title.is_empty() || !is_valid_session_id(session_id) {
        return;
    }

    let result = match tool {
        "claude" => sync_claude_title(project_path, session_id, &clean_title, host_env),
        "codex" => sync_codex_title(session_id, &clean_title),
        "antigravity" => sync_antigravity_title(session_id, &clean_title),
        _ => Ok(false),
    };

    match result {
        Ok(true) => tracing::debug!(
            target: "session.agent_title",
            tool,
            session_id,
            title = %clean_title,
            "synced native agent chat title"
        ),
        Ok(false) => {}
        Err(err) => tracing::debug!(
            target: "session.agent_title",
            tool,
            session_id,
            error = %err,
            "skipped native agent chat title sync"
        ),
    }
}

/// Append a `custom-title` record to Claude Code's session transcript JSONL
/// (`<claude_home>/projects/<encoded-cwd>/<session_id>.jsonl`) when the
/// transcript already exists on disk.
fn sync_claude_title(
    project_path: &str,
    session_id: &str,
    title: &str,
    host_env: &[String],
) -> anyhow::Result<bool> {
    let claude_home = claude_home_for_host_environment(host_env)?;
    let canonical = canonicalize_or_raw(project_path);
    let encoded = encode_claude_project_path(&canonical.to_string_lossy());
    let transcript = claude_home
        .join("projects")
        .join(encoded)
        .join(format!("{session_id}.jsonl"));
    sync_claude_title_at(&transcript, session_id, title)
}

fn sync_claude_title_at(
    transcript_path: &Path,
    session_id: &str,
    title: &str,
) -> anyhow::Result<bool> {
    if !transcript_path.is_file() {
        return Ok(false);
    }
    let record = serde_json::json!({
        "type": "custom-title",
        "customTitle": title,
        "sessionId": session_id,
    });
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    let mut file = OpenOptions::new().append(true).open(transcript_path)?;
    file.write_all(line.as_bytes())?;
    Ok(true)
}

/// Append a thread-name entry to Codex CLI's append-only
/// `<codex_home>/session_index.jsonl` when `<codex_home>/sessions` exists.
fn sync_codex_title(session_id: &str, title: &str) -> anyhow::Result<bool> {
    let codex_home = resolve_agent_home(Some("CODEX_HOME"), ".codex")?;
    sync_codex_title_at(&codex_home, session_id, title)
}

fn sync_codex_title_at(codex_home: &Path, session_id: &str, title: &str) -> anyhow::Result<bool> {
    if !codex_home.join("sessions").is_dir() {
        return Ok(false);
    }
    let index_path = codex_home.join("session_index.jsonl");
    let record = serde_json::json!({
        "id": session_id,
        "thread_name": title,
        "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    });
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)?;
    file.write_all(line.as_bytes())?;
    Ok(true)
}

/// Resolve Antigravity CLI's data root (`ANTIGRAVITY_GEMINI_DIR` or
/// `~/.gemini/antigravity-cli`).
fn antigravity_cli_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ANTIGRAVITY_GEMINI_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".gemini").join("antigravity-cli"))
}

/// Update `<agy_home>/annotations/<session_id>.pbtxt` with `title:"<escaped>"`
/// when `<agy_home>/conversations/<session_id>.db` exists, preserving any other
/// annotation fields (`archived`, `pinned`, `starred`, `tags`, etc.).
fn sync_antigravity_title(session_id: &str, title: &str) -> anyhow::Result<bool> {
    let Some(home) = antigravity_cli_home() else {
        return Ok(false);
    };
    sync_antigravity_title_at(&home, session_id, title)
}

fn sync_antigravity_title_at(
    agy_home: &Path,
    session_id: &str,
    title: &str,
) -> anyhow::Result<bool> {
    if !agy_home
        .join("conversations")
        .join(format!("{session_id}.db"))
        .is_file()
    {
        return Ok(false);
    }
    let annotations_dir = agy_home.join("annotations");
    fs::create_dir_all(&annotations_dir)?;
    let path = annotations_dir.join(format!("{session_id}.pbtxt"));
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let updated = upsert_pbtxt_title(&existing, title);

    let tmp_path = annotations_dir.join(format!(".{session_id}.pbtxt.tmp"));
    fs::write(&tmp_path, updated)?;
    fs::rename(&tmp_path, &path)?;
    Ok(true)
}

/// Escape `value` for a double-quoted protobuf text format string literal.
fn escape_pbtxt_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Replace an existing `title:` field or insert one while preserving every
/// other line in an Antigravity annotation `.pbtxt` file.
pub(crate) fn upsert_pbtxt_title(existing: &str, title: &str) -> String {
    let title_line = format!("title:\"{}\"", escape_pbtxt_string(title));
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for raw_line in existing.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("title:") {
            if !replaced {
                lines.push(title_line.clone());
                replaced = true;
            }
        } else if !raw_line.trim().is_empty() {
            lines.push(raw_line.to_string());
        }
    }
    if !replaced {
        lines.insert(0, title_line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_agent_title_collapses_whitespace_and_control_chars() {
        assert_eq!(sanitize_agent_title("  fix\n\tauth\rbug  "), "fix auth bug");
        assert_eq!(sanitize_agent_title("   \n\t  "), "");
    }

    #[test]
    fn command_has_name_flag_detects_short_and_long_forms() {
        assert!(command_has_name_flag("claude --name foo"));
        assert!(command_has_name_flag("claude --name=foo"));
        assert!(command_has_name_flag("claude -n foo"));
        assert!(command_has_name_flag("claude -nfoo"));
        assert!(!command_has_name_flag("claude --resume abc --fork-session"));
        assert!(!command_has_name_flag("claude --model sonnet"));
    }

    #[test]
    fn sync_claude_title_appends_custom_title_only_when_transcript_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp
            .path()
            .join("11111111-1111-4111-8111-111111111111.jsonl");
        assert!(!sync_claude_title_at(
            &transcript,
            "11111111-1111-4111-8111-111111111111",
            "Auth refactor"
        )
        .unwrap());
        assert!(!transcript.exists());

        fs::write(&transcript, "{\"type\":\"mode\",\"mode\":\"normal\"}\n").unwrap();
        assert!(sync_claude_title_at(
            &transcript,
            "11111111-1111-4111-8111-111111111111",
            "Auth refactor"
        )
        .unwrap());
        let content = fs::read_to_string(&transcript).unwrap();
        let last: serde_json::Value =
            serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "custom-title");
        assert_eq!(last["customTitle"], "Auth refactor");
        assert_eq!(last["sessionId"], "11111111-1111-4111-8111-111111111111");
    }

    #[test]
    fn sync_codex_title_appends_session_index_entry() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!sync_codex_title_at(tmp.path(), "codex-uuid-1234", "Forked Thread").unwrap());
        fs::create_dir_all(tmp.path().join("sessions")).unwrap();

        assert!(sync_codex_title_at(tmp.path(), "codex-uuid-1234", "Forked Thread").unwrap());
        let content = fs::read_to_string(tmp.path().join("session_index.jsonl")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["id"], "codex-uuid-1234");
        assert_eq!(parsed["thread_name"], "Forked Thread");
        assert!(parsed["updated_at"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn sync_antigravity_title_upserts_pbtxt_and_preserves_other_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "6c3ebf2d-f322-4081-980e-81da3778e095";
        assert!(!sync_antigravity_title_at(tmp.path(), sid, "New Name").unwrap());

        fs::create_dir_all(tmp.path().join("conversations")).unwrap();
        fs::create_dir_all(tmp.path().join("annotations")).unwrap();
        fs::write(
            tmp.path().join("conversations").join(format!("{sid}.db")),
            b"",
        )
        .unwrap();
        fs::write(
            tmp.path().join("annotations").join(format!("{sid}.pbtxt")),
            "title:\"Old Name\"\npinned:true\nstarred:true\n",
        )
        .unwrap();

        assert!(sync_antigravity_title_at(tmp.path(), sid, "Fork \"Alpha\" \\ 1").unwrap());
        let content =
            fs::read_to_string(tmp.path().join("annotations").join(format!("{sid}.pbtxt")))
                .unwrap();
        assert_eq!(
            content,
            "title:\"Fork \\\"Alpha\\\" \\\\ 1\"\npinned:true\nstarred:true"
        );
    }
}

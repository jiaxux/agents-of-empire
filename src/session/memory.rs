//! Bounded, on-demand conversation memory for cross-session context sharing.
//!
//! This module performs no startup work and is never called from rendering or
//! ordinary input forwarding. The TUI invokes it only from its background
//! memory worker after the user explicitly opens a session's context.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Instance;

const MAX_TRANSCRIPT_READ_BYTES: u64 = 256 * 1024;
const MAX_MEMORY_BYTES: usize = 24 * 1024;
const MAX_ASSISTANT_TURN_BYTES: usize = 8 * 1024;
const PANE_HISTORY_LINES: usize = 800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    StructuredEvents,
    ClaudeTranscript,
    CodexTranscript,
    PaneHistory,
}

impl MemorySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::StructuredEvents => "ACP transcript",
            Self::ClaudeTranscript => "Claude transcript",
            Self::CodexTranscript => "Codex transcript",
            Self::PaneHistory => "recent pane history",
        }
    }

    /// Whether this source has a reliable assistant-turn boundary. Generic
    /// pane history is useful to read and share, but cannot safely drive an
    /// autonomous relay because prompts, tools, and replies are interleaved.
    pub fn supports_dialogue(self) -> bool {
        !matches!(self, Self::PaneHistory)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemory {
    pub session_id: String,
    pub title: String,
    pub tool: String,
    pub source: MemorySource,
    pub content: String,
    /// True when the source is intentionally bounded or may not contain the
    /// full conversation. All current providers are bounded by design.
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestAssistantTurn {
    pub source: MemorySource,
    pub content: Option<String>,
    /// Identity of the native assistant record or final ACP chunk. AoE uses
    /// this instead of reply text so two legitimate identical replies still
    /// count as distinct turns.
    pub marker: Option<u64>,
}

/// Cheap metadata-only capability check used by setup dialogs. It performs no
/// filesystem access and does not promise that a transcript already exists.
pub fn supports_automatic_dialogue(instance: &Instance) -> bool {
    #[cfg(feature = "serve")]
    if instance.is_structured() {
        return true;
    }
    !instance.is_sandboxed() && matches!(instance.tool.as_str(), "claude" | "codex")
}

/// Read the most recent bounded assistant turn from a source with reliable
/// role boundaries. This is called only by the lazy background memory worker.
pub fn latest_assistant_turn(instance: &Instance) -> Result<LatestAssistantTurn> {
    #[cfg(feature = "serve")]
    if instance.is_structured() {
        let events = load_structured_event_rows(&instance.id)?;
        let snapshot = latest_structured_assistant_snapshot(&events);
        return Ok(LatestAssistantTurn {
            source: MemorySource::StructuredEvents,
            content: snapshot.as_ref().map(|(content, _)| content.clone()),
            marker: snapshot.map(|(_, marker)| marker),
        });
    }

    if instance.is_sandboxed() {
        anyhow::bail!(
            "'{}' only exposes pane history; automatic dialogue needs a native transcript",
            instance.title
        );
    }

    let (source, tail, extract) = match instance.tool.as_str() {
        "claude" => (
            MemorySource::ClaudeTranscript,
            load_claude_transcript_tail(instance)?,
            extract_claude_message as fn(&serde_json::Value) -> Option<(Role, String)>,
        ),
        "codex" => (
            MemorySource::CodexTranscript,
            load_codex_transcript_tail(instance)?,
            extract_codex_message as fn(&serde_json::Value) -> Option<(Role, String)>,
        ),
        _ => anyhow::bail!(
            "'{}' has no supported native transcript for automatic dialogue",
            instance.title
        ),
    };
    let snapshot = tail.and_then(|tail| latest_jsonl_assistant_snapshot(&tail, extract));
    Ok(LatestAssistantTurn {
        source,
        content: snapshot.as_ref().map(|(content, _)| content.clone()),
        marker: snapshot.map(|(_, marker)| marker),
    })
}

/// Load a bounded read-only snapshot for `instance`.
pub fn load(instance: &Instance) -> Result<SessionMemory> {
    #[cfg(feature = "serve")]
    if instance.is_structured() {
        if let Some(content) = load_structured_events(&instance.id)? {
            return Ok(snapshot(instance, MemorySource::StructuredEvents, content));
        }
        anyhow::bail!(
            "No recorded conversation memory was found for '{}'.",
            instance.title
        );
    }

    if !instance.is_sandboxed() {
        if instance.tool == "claude" {
            if let Some(content) = load_claude_transcript(instance)? {
                return Ok(snapshot(instance, MemorySource::ClaudeTranscript, content));
            }
        } else if instance.tool == "codex" {
            if let Some(content) = load_codex_transcript(instance)? {
                return Ok(snapshot(instance, MemorySource::CodexTranscript, content));
            }
        }
    }

    let tmux = crate::tmux::Session::new(&instance.id, &instance.title)
        .context("resolve the session's tmux pane")?;
    let raw = tmux
        .capture_pane(PANE_HISTORY_LINES)
        .context("capture recent pane history")?;
    let content = crate::session::smart_rename::strip_ansi(&raw);
    let content = content.trim().to_string();
    if content.is_empty() {
        anyhow::bail!(
            "No stored transcript or live pane history was found for '{}'.",
            instance.title
        );
    }
    Ok(snapshot(
        instance,
        MemorySource::PaneHistory,
        cap_tail(&content, MAX_MEMORY_BYTES),
    ))
}

fn snapshot(instance: &Instance, source: MemorySource, content: String) -> SessionMemory {
    SessionMemory {
        session_id: instance.id.clone(),
        title: instance.title.clone(),
        tool: instance.tool.clone(),
        source,
        content: cap_tail(content.trim(), MAX_MEMORY_BYTES),
        partial: true,
    }
}

fn load_claude_transcript(instance: &Instance) -> Result<Option<String>> {
    let Some(tail) = load_claude_transcript_tail(instance)? else {
        return Ok(None);
    };
    Ok(render_jsonl_memory(&tail, extract_claude_message))
}

fn load_claude_transcript_tail(instance: &Instance) -> Result<Option<String>> {
    let Some(session_id) = instance.agent_session_id.as_deref() else {
        return Ok(None);
    };
    let home = match super::capture::resolve_agent_home(Some("CLAUDE_CONFIG_DIR"), ".claude") {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let canonical = super::capture::canonicalize_or_raw(&instance.project_path);
    let encoded = super::capture::encode_claude_project_path(&canonical.to_string_lossy());
    let path = home
        .join("projects")
        .join(encoded)
        .join(format!("{session_id}.jsonl"));
    if !path.is_file() {
        return Ok(None);
    }
    read_file_tail(&path, MAX_TRANSCRIPT_READ_BYTES).map(Some)
}

fn load_codex_transcript(instance: &Instance) -> Result<Option<String>> {
    let Some(tail) = load_codex_transcript_tail(instance)? else {
        return Ok(None);
    };
    Ok(render_jsonl_memory(&tail, extract_codex_message))
}

fn load_codex_transcript_tail(instance: &Instance) -> Result<Option<String>> {
    let Some(session_id) = instance.agent_session_id.as_deref() else {
        return Ok(None);
    };
    let home = match super::capture::resolve_agent_home(Some("CODEX_HOME"), ".codex") {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let sessions = home.join("sessions");
    let Some(path) = find_codex_transcript(&sessions, session_id, 0) else {
        return Ok(None);
    };
    read_file_tail(&path, MAX_TRANSCRIPT_READ_BYTES).map(Some)
}

fn find_codex_transcript(dir: &Path, session_id: &str, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    for entry in super::capture::resilient_read_dir(dir).ok()? {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
                if let Some(found) = find_codex_transcript(&path, session_id, depth + 1) {
                    return Some(found);
                }
            }
        } else if path.extension().and_then(|v| v.to_str()) == Some("jsonl")
            && path
                .file_stem()
                .and_then(|v| v.to_str())
                .is_some_and(|stem| stem.ends_with(session_id))
        {
            return Some(path);
        }
    }
    None
}

fn read_file_tail(path: &Path, max_bytes: u64) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut bytes = Vec::with_capacity((len - start).min(max_bytes) as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if start > 0 {
        if let Some(newline) = bytes.iter().position(|b| *b == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Assistant => "Assistant",
        }
    }
}

fn render_jsonl_memory(
    jsonl: &str,
    extract: fn(&serde_json::Value) -> Option<(Role, String)>,
) -> Option<String> {
    let mut messages: Vec<(Role, String)> = Vec::new();
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some((role, text)) = extract(&value) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || is_command_wrapper(text) {
            continue;
        }
        if messages
            .last()
            .is_some_and(|(last_role, last)| *last_role == role && last == text)
        {
            continue;
        }
        messages.push((role, text.to_string()));
    }
    if messages.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (role, text) in messages {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push('[');
        out.push_str(role.label());
        out.push_str("]\n");
        out.push_str(&text);
        out.push('\n');
    }
    Some(cap_tail(out.trim(), MAX_MEMORY_BYTES))
}

#[cfg(test)]
fn latest_jsonl_assistant_turn(
    jsonl: &str,
    extract: fn(&serde_json::Value) -> Option<(Role, String)>,
) -> Option<String> {
    latest_jsonl_assistant_snapshot(jsonl, extract).map(|(content, _)| content)
}

fn latest_jsonl_assistant_snapshot(
    jsonl: &str,
    extract: fn(&serde_json::Value) -> Option<(Role, String)>,
) -> Option<(String, u64)> {
    use std::hash::{Hash, Hasher};

    let mut latest = None;
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some((role, text)) = extract(&value) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || is_command_wrapper(text) {
            continue;
        }
        match role {
            Role::User => latest = None,
            Role::Assistant => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                line.hash(&mut hasher);
                latest = Some((cap_tail(text, MAX_ASSISTANT_TURN_BYTES), hasher.finish()));
            }
        }
    }
    latest
}

fn extract_claude_message(value: &serde_json::Value) -> Option<(Role, String)> {
    let role = match value.get("type")?.as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let content = value.get("message")?.get("content")?;
    let text = extract_content_text(content)?;
    Some((role, text))
}

fn extract_codex_message(value: &serde_json::Value) -> Option<(Role, String)> {
    let record_type = value.get("type").and_then(|v| v.as_str());
    let payload = value.get("payload")?;

    if record_type == Some("response_item") && payload.get("type")?.as_str()? == "message" {
        let role = parse_role(payload.get("role")?.as_str()?)?;
        return Some((role, extract_content_text(payload.get("content")?)?));
    }

    if record_type == Some("event_msg") {
        let role = match payload.get("type")?.as_str()? {
            "user_message" => Role::User,
            "agent_message" => Role::Assistant,
            _ => return None,
        };
        let content = payload
            .get("message")
            .or_else(|| payload.get("text"))
            .or_else(|| payload.get("content"))?;
        return Some((role, extract_content_text(content)?));
    }

    None
}

fn parse_role(role: &str) -> Option<Role> {
    match role {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn extract_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                let kind = part.get("type").and_then(|v| v.as_str());
                if !matches!(kind, None | Some("text" | "input_text" | "output_text")) {
                    continue;
                }
                if let Some(text) = part
                    .get("text")
                    .or_else(|| part.get("content"))
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                {
                    out.push(text);
                }
            }
            (!out.is_empty()).then(|| out.join("\n"))
        }
        _ => None,
    }
}

fn is_command_wrapper(text: &str) -> bool {
    text.starts_with("<local-command-") || text.starts_with("<command-")
}

fn cap_tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    let tail = tail
        .find('\n')
        .map(|newline| &tail[newline + 1..])
        .unwrap_or(tail);
    format!("[Earlier memory omitted from this bounded snapshot]\n{tail}")
}

#[cfg(feature = "serve")]
fn load_structured_events(session_id: &str) -> Result<Option<String>> {
    let events = load_structured_event_rows(session_id)?;
    if events.is_empty() {
        return Ok(None);
    }

    let (summary, summarized_until) = super::conversation_summary::last_summary(&events);
    let (recent, _, _) =
        super::conversation_summary::extract_transcript_delta(&events, summarized_until);
    let mut content = String::new();
    if let Some(summary) = summary {
        content.push_str("[Summary so far]\n");
        content.push_str(summary.trim());
        content.push_str("\n\n");
    }
    if !recent.trim().is_empty() {
        content.push_str("[Recent activity]\n");
        content.push_str(recent.trim());
    }
    if content.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(cap_tail(content.trim(), MAX_MEMORY_BYTES)))
}

#[cfg(feature = "serve")]
fn load_structured_event_rows(session_id: &str) -> Result<Vec<(u64, crate::acp::state::Event)>> {
    use rusqlite::{params, Connection, OpenFlags};

    const EVENT_LIMIT: usize = 800;
    let db_path = super::get_app_dir()?.join("acp_events.db");
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open read-only ACP memory at {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(250))?;
    let mut stmt = match conn.prepare(
        "SELECT seq, event_json FROM acp_events
         WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
    ) {
        Ok(stmt) => stmt,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(Vec::new())
        }
        Err(error) => return Err(error.into()),
    };
    let rows = stmt.query_map(params![session_id, EVENT_LIMIT as i64], |row| {
        let seq = row.get::<_, i64>(0)?.max(0) as u64;
        Ok((seq, row.get::<_, String>(1)?))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (seq, json) = row?;
        if let Ok(event) = serde_json::from_str::<crate::acp::state::Event>(&json) {
            events.push((seq, event));
        }
    }
    events.reverse();
    Ok(events)
}

#[cfg(all(feature = "serve", test))]
fn latest_structured_assistant_turn(events: &[(u64, crate::acp::state::Event)]) -> Option<String> {
    latest_structured_assistant_snapshot(events).map(|(content, _)| content)
}

#[cfg(feature = "serve")]
fn latest_structured_assistant_snapshot(
    events: &[(u64, crate::acp::state::Event)],
) -> Option<(String, u64)> {
    use crate::acp::state::Event;

    let mut reply = String::new();
    let mut saw_prompt = false;
    let mut marker = None;
    for (seq, event) in events {
        match event {
            Event::UserPromptSent { .. } => {
                saw_prompt = true;
                reply.clear();
                marker = None;
            }
            Event::AgentMessageChunk { text } if saw_prompt => {
                reply.push_str(text);
                marker = Some(*seq);
            }
            _ => {}
        }
    }
    let reply = reply.trim();
    marker
        .filter(|_| !reply.is_empty())
        .map(|marker| (cap_tail(reply, MAX_ASSISTANT_TURN_BYTES), marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_extractors_keep_conversation_text_and_drop_noise() {
        let claude = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"Fix it\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done\"},{\"type\":\"tool_use\",\"text\":\"noise\"}]}}\n"
        );
        let rendered = render_jsonl_memory(claude, extract_claude_message).unwrap();
        assert_eq!(rendered, "[User]\nFix it\n\n[Assistant]\nDone");

        let codex = concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Check tests\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"All pass\"}]}}\n"
        );
        let rendered = render_jsonl_memory(codex, extract_codex_message).unwrap();
        assert_eq!(rendered, "[User]\nCheck tests\n\n[Assistant]\nAll pass");
    }

    #[test]
    fn latest_assistant_turn_uses_the_reply_after_the_latest_user_message() {
        let transcript = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"first\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":\"old reply\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"second\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":\"new reply\"}}\n"
        );
        assert_eq!(
            latest_jsonl_assistant_turn(transcript, extract_claude_message).as_deref(),
            Some("new reply")
        );

        let waiting = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":\"old reply\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"still running\"}}\n"
        );
        assert_eq!(
            latest_jsonl_assistant_turn(waiting, extract_claude_message),
            None
        );
    }

    #[test]
    fn native_turn_marker_distinguishes_identical_reply_text() {
        let old = concat!(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":\"first\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"message\":{\"content\":\"same\"}}\n"
        );
        let newer = concat!(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":\"first\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"message\":{\"content\":\"same\"}}\n",
            "{\"type\":\"user\",\"uuid\":\"u2\",\"message\":{\"content\":\"second\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"a2\",\"message\":{\"content\":\"same\"}}\n"
        );
        let old = latest_jsonl_assistant_snapshot(old, extract_claude_message).unwrap();
        let newer = latest_jsonl_assistant_snapshot(newer, extract_claude_message).unwrap();
        assert_eq!(old.0, newer.0);
        assert_ne!(old.1, newer.1);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn structured_latest_turn_coalesces_only_chunks_after_latest_prompt() {
        use crate::acp::state::Event;

        let prompt = |text: &str| Event::UserPromptSent {
            prompt_id: None,
            text: text.to_string(),
            attachments: Vec::new(),
        };
        let events = vec![
            (1, prompt("first")),
            (
                2,
                Event::AgentMessageChunk {
                    text: "old".to_string(),
                },
            ),
            (3, prompt("second")),
            (
                4,
                Event::AgentMessageChunk {
                    text: "new ".to_string(),
                },
            ),
            (
                5,
                Event::AgentMessageChunk {
                    text: "reply".to_string(),
                },
            ),
        ];
        assert_eq!(
            latest_structured_assistant_turn(&events).as_deref(),
            Some("new reply")
        );
    }

    #[test]
    fn bounded_memory_keeps_valid_utf8_tail() {
        let text = format!("old\n{}new", "é".repeat(64));
        let capped = cap_tail(&text, 17);
        assert!(capped.starts_with("[Earlier memory omitted"));
        assert!(capped.ends_with("new"));
    }

    #[test]
    fn load_prefers_claude_native_transcript() {
        let temp = tempfile::TempDir::new().unwrap();
        let _env =
            crate::session::test_support::EnvGuard::set(&[("CLAUDE_CONFIG_DIR", temp.path())]);
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let canonical =
            super::super::capture::canonicalize_or_raw(project.to_string_lossy().as_ref());
        let encoded =
            super::super::capture::encode_claude_project_path(&canonical.to_string_lossy());
        let session_id = "memory-claude-session";
        let transcript_dir = temp.path().join("projects").join(encoded);
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(
            transcript_dir.join(format!("{session_id}.jsonl")),
            "{\"type\":\"user\",\"message\":{\"content\":\"Native context\"}}\n",
        )
        .unwrap();

        let mut instance = Instance::new("source", project.to_string_lossy().as_ref());
        instance.tool = "claude".to_string();
        instance.agent_session_id = Some(session_id.to_string());
        let memory = load(&instance).unwrap();
        assert_eq!(memory.source, MemorySource::ClaudeTranscript);
        assert_eq!(memory.content, "[User]\nNative context");
    }

    #[test]
    fn load_finds_codex_transcript_in_date_tree() {
        let temp = tempfile::TempDir::new().unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[("CODEX_HOME", temp.path())]);
        let session_id = "memory-codex-session";
        let transcript_dir = temp.path().join("sessions/2026/08/14");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(
            transcript_dir.join(format!("rollout-{session_id}.jsonl")),
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Codex context\"}}\n",
        )
        .unwrap();

        let mut instance = Instance::new("source", temp.path().to_string_lossy().as_ref());
        instance.tool = "codex".to_string();
        instance.agent_session_id = Some(session_id.to_string());
        let memory = load(&instance).unwrap();
        assert_eq!(memory.source, MemorySource::CodexTranscript);
        assert_eq!(memory.content, "[Assistant]\nCodex context");
    }
}

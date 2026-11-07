# Approval Autopilot

Approval Autopilot is an opt-in AoE mechanism for terminal sessions. When an
agent displays a verified tool-permission prompt, AoE prefers the menu's
session-scoped approval from outside the agent. Codex edit prompts receive `a`,
Codex command prompts receive `p`, and Claude receives its mapped persistent
choice when the complete option is visible. AoE confirms Codex's selected
choice with `Enter`; menus without a persistent choice fall back to allow once.
AoE does not modify Claude Code, Codex, OMP, or their configuration.

Enable it from Settings by searching for **Approval Autopilot**, or add this to
the global AoE config:

```toml
[session]
approval_autopilot = true
```

The setting takes effect live in the TUI and in `aoe serve`. At least one of
those processes must be running because AoE, rather than the agent, observes
and answers the prompt.

## Safety boundary

The responder is intentionally narrower than AoE's `Waiting` status:

- Claude Code tool approvals, Codex command and file-edit approvals, and OMP
  tool approvals are supported when their complete known prompt shape is
  visible. Codex command menus may use either the stock two-choice or
  three-choice layout.
- Ordinary chat confirmations, Claude `AskUserQuestion`, Codex plan questions,
  folder-trust prompts, and incomplete or unknown menus are never answered.
- Non-interactive policy rejections are not approval prompts and cannot be
  answered by Autopilot.
- A complete known persistent choice is preferred so the same file or command
  does not ask again during that agent session. A verified two-choice menu, or
  an agent without a persistent choice, falls back to **allow once**.
- The TUI and daemon share a per-session lock and hashed prompt marker, so two
  AoE processes cannot approve the same visible request twice. Only an exact
  repeated prompt is suppressed; a different verified request can follow
  immediately without an artificial delay.
- No pane or prompt text is stored. The marker contains only a SHA-256
  fingerprint and timestamp.

This is different from `yolo_mode_default`. YOLO starts a new agent with its
own permission bypass enabled. Approval Autopilot leaves the agent's normal
permission checks in place and responds only after a recognized prompt appears.

## Performance and typing

When disabled, Approval Autopilot performs no pane capture, prompt parse,
filesystem lock, or config read. When enabled, it runs only on AoE's existing
background status workers, reuses the pane snapshot status detection already
needed, and rejects ordinary waiting panes through cheap exact-label gates.
Marker I/O and key dispatch occur once per verified permission event. Nothing
is added to rendering, key translation, live-send batching, or paste handling.

OpenCode still supports the manual quick-response path, but its terminal prompt
does not yet have a strict enough detector for unattended approval. Select the
session, press `a`, then press `a` again to allow once. The same manual fallback
remains available for every mapped agent even when Approval Autopilot is off.

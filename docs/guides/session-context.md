# Session Context, Sharing, and Agent Dialogue

Agent of Empires can open a bounded, read-only snapshot of one session and
send it to another session. This is useful when one agent has already found a
root cause, test result, or implementation detail that another agent needs.

## Keyboard workflow

From the session list:

| Key | Action |
|-----|--------|
| `y` | Open context for the selected session |
| `Y` | Open context when strict hotkeys are enabled |
| `j` / `k` | Scroll the context viewer or target picker |
| `g` / `G` | Jump to the beginning or end |
| `s` | Open the share target picker |
| `c` | Start or reopen a supervised agent dialogue |
| `Enter` | Confirm the selected target and send |
| `Esc` | Go back or close |

The same viewer is available through **Open session context / memory** in the
`Ctrl+K` command palette and **Open context** in a session's right-click menu.
The palette also has **Start or view agent dialogue**, and the right-click
menu has **Agent dialogue**. These entries use the same setup path as `y`, then
`c`.

The `y` shortcut belongs only to the home session list. In live-send, `y`
remains ordinary text and is forwarded to the focused agent. This keeps the
typing path predictable for Vim users.

## What the snapshot contains

AoE chooses the best available source only after the viewer is opened:

- Structured sessions use their recorded ACP conversation events.
- Host Claude and Codex sessions use their native transcript when available.
- Other terminal and sandbox sessions fall back to recent tmux pane history.

The viewer keeps at most 24 KiB of recent context. A share sends at most 16 KiB
and includes the source session, agent, session id, and memory-source label.
The receiving agent is explicitly told that the material is potentially stale
background context, not new user instructions.

The `s` workflow remains user-directed, one-way sharing.

## Supervised agent dialogue

Press `c` in a native context viewer, choose a second session with `j` / `k`,
press `Enter`, and type a concrete objective. The selected session speaks
first. AoE then relays only the newest bounded assistant reply to the other
session. It never re-sends the full transcript, which keeps token growth
linear and makes the source of each turn unambiguous.

The dialogue viewer provides these controls:

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll replies |
| `g` / `G` | Jump to the beginning or live end |
| `p` | Pause or resume the relay |
| `x` | Stop the relay |
| `+` | Add two replies to the turn budget |
| `Esc` | Close the viewer while the relay continues |

AoE starts with a six-reply budget. The user can extend it to a hard maximum
of 20 replies. Each captured reply is limited to 8 KiB, all replies together
are limited to 64 KiB, the objective is limited to 4 KiB, and the dialogue
expires after 30 minutes. Only one dialogue can be active in the TUI at a
time, and only one participant is prompted at a time.

Automatic capture requires a reliable role boundary, so it supports recorded
ACP events and native host Claude or Codex transcripts. Pane-history fallback
remains available for reading and one-way sharing but cannot drive a dialogue.
Both participants must be idle or stopped at setup time.

Opening live-send or a manual message composer for either participant pauses
the relay. A permission prompt, user question, stopped session, or error also
pauses it. This prevents human and relay messages from being interleaved. Use
`p` in the dialogue viewer after the participant is ready to continue.

## Performance behavior

The provider is lazy: no transcript scan, SQLite query, tmux capture, worker
thread, or extra polling starts until `y`, the palette action, or the context
menu action is used. Loading, sending, and dialogue reply capture run on one
background worker. Dialogue turns advance from AoE's existing session-status
events. There is no dialogue timer or recurring repaint loop.

Normal home navigation, preview rendering, composer typing, and live-send
typing do not parse dialogue state, perform memory I/O, or wait for the relay.
The viewer wraps content only when its width or dialogue revision changes.

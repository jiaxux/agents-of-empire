# Agent of Empires: Local Patch Summary

This document covers all changes made to the `agent-of-empires` fork at
`~/agent-of-empires`, including the preview-selection stability, Claude
renderer, and pane-focus fixes.

## 1. Applied Changes

### October 2026 typing responsiveness

- Periodic session-ID poller repair checks the background tmux snapshot before
  probing a session. Inactive rows no longer fork `tmux list-sessions` on the
  input thread every 500 ms. A missing or expired snapshot defers repair until
  a status refresh; ordinary session startup still probes immediately.
- Cached candidates still pass the existing live-pane and unique-agent checks.
  Renamed sessions remain discoverable by ID, and paired terminals cannot seed
  agent pollers. Keyboard bindings, live-send transport, and frame pacing are
  unchanged.
- Regression tests cover inactive rows, unavailable and expired caches, renamed
  agents, and replacement of a stopped poller when its agent becomes available.
  The isolated `scripts/benchmark-live-send.py` measures key delivery and visible
  echo using a private tmux server and temporary application state.

Reproduce the profile-sized typing workload with an optimized build:

```sh
cargo build --profile dev-release --features serve --locked --offline -j2
python3 scripts/benchmark-live-send.py --binary target/dev-release/aoe \
  --idle-sessions 100 --samples 40 --gap-ms 300 --assert-responsive
```

Measured on this Mac with the same optimized build profile, a 160 by 48
terminal, VT live preview, one echo agent, and 100 stopped sessions. Neither
run overlapped a build. Times are milliseconds, measured through real PTYs:

| Measurement | Before | After |
| --- | ---: | ---: |
| Median visible echo | 103.52 | 1.24 |
| 95th-percentile visible echo | 827.07 | 34.77 |
| Worst visible echo | 898.05 | 42.32 |
| 95th-percentile key delivery | 824.02 | 29.51 |

These measurements cover AoE's local input and display latency with a synthetic
agent, not model response time or remote network latency.

Validation: formatting passed; Clippy with all targets and `serve` passed with
the same 12 existing warnings. The Rust library suite reported 6,519 passed and
4 ignored. One unrelated process-group timeout test failed waiting for its
SIGTERM trap and passed unchanged when rerun alone. Both new regression tests
and the real-tmux poller replacement test passed in the full suite.

The release build with `serve` is installed through the existing
`/opt/homebrew/bin/aoe` symlink. Its separate 100-session smoke checks passed:
VT preview had 36.16 ms p95 echo and 39.70 ms maximum; capture fallback had
30.95 ms p95 and 37.66 ms maximum. The fallback fixture explicitly sets
`[tmux] vt_live = false`. Reopen the AoE frontend to activate the rebuild;
running agent sessions do not need to restart. The previous installed binary
is backed up at `/private/tmp/aoe-installed-before-typing-20261009`.

### September 2026 selective upstream backports

- Abandoned session-ID pollers stop after confirmed pane loss, with a startup
  grace and transient-error protection. A paired terminal or ambiguous agent
  name cannot seed a poller. Full-budget repair skips expensive spawn setup.
- Agent discovery uses bounded process-group probes with closed stdin and a
  shared 60-second cache. Settings searches reuse cached availability;
  keyboard and mouse Recheck both invalidate it before probing again.
- The host terminal tab follows the selected chat. Disable **Host Tab Title**
  in Interaction settings to keep terminal-managed naming. Updates are deduplicated
  in memory; no new input-path filesystem or tmux queries are added.
- `aoe session rename SESSION --branch NEW_BRANCH` renames a managed branch
  without moving its worktree. Shared and protected branches are guarded, and
  failed metadata persistence attempts a checked rollback.
- Send Message (`m`, or the configured binding) opens the structured composer.
  Buffered paste is retained per target and consumed once after that target mounts.
- Structured-view defaults support opt-in `pin_model = true`. Creation and
  worker restart use the pin; explicit per-session thinking effort survives.
  Plugin creation rejects off-pin models. Web defaults include a pin checkbox;
  the TUI's existing JSON defaults editor and config file support the same field.
- Browser terminal selections hold the painted frame until cleared or **Back to
  live** is pressed. Stream reception continues, and newly exposed history is
  retained without replacing selected text. Stable row keys avoid remounts.

Sources: upstream #3872, #3884, #3905, #3874, #3909, #3927, #3911, #3868,
#3906, #3683, #3903, and #3847, adapted to this fork's older module layout.
No schema migration, bulk upstream merge, Autopilot change, or tmux input
transport change is included. Existing local changes are preserved.

Model-pin example (global or profile configuration):

```toml
[acp.acp_defaults.claude]
model = "YOUR_ADVERTISED_MODEL_ID"
pin_model = true
```

Pins do not change an already-running conversation's model. They are not a
live model-switch restriction. Terminal-only agent sessions are unaffected.

Verification for these backports:

- Rust library suite with `serve`: 6,518 passed, 4 ignored.
- Model/effort integration: 3 passed. Real tmux paste and both sidebar
  right-click checks passed through the Cargo e2e runner. Direct right-click
  tests failed at startup inside the sandbox and passed unchanged outside it.
- Browser-terminal unit tests: 77 passed, including typing, paste, selection,
  and touch. Model-default widget tests and both real-browser selection tests
  passed. Web types, formatting, lint, and coverage-matrix validation passed.
- All-target build checks passed both with `serve` and without default
  features. A missing `serve` guard found during verification was corrected
  without changing the full build's behavior.
- Strict Clippy remains blocked by 12 pre-existing warnings outside the new
  behavior; unrelated code has not been rewritten to suppress them.

Builds used two Cargo jobs to limit temporary laptop load. These checks are
not a before/after CPU or memory benchmark of a long-running user session.
The existing shortcuts and live-input transport are retained; availability
caching, poller cleanup, and stable browser row identity target avoidable work.

At the initial handoff these were source changes only. The September 29
checkpoint saves them before the TUI redesign; no push, installed-binary
replacement, or live session restart is included. To activate after review, build with
`cargo build --release --features serve -j2` from this repository, then reopen
the AoE frontend. An already-running dashboard daemon also needs a separate
planned restart to pick up backend changes.

### 1.1 Vim-style pane navigation (`Ctrl+H` / `Ctrl+L`)

**File:** `src/tui/home/input.rs`  
**Goal:** Mirror neovim/vim-tmux-navigator muscle memory. `Ctrl+L` jumps
focus to the right terminal pane, `Ctrl+H` jumps back to the left session list.

**Background:** AoE has no config-file keybinding system for TUI actions.
All bindings are compiled in. The only existing way to "focus" the right pane
is `Tab` (enters live-send mode) and `Ctrl+Q` (exits it). This patch adds
`Ctrl+H`/`Ctrl+L` as aliases.

#### Change A: `Ctrl+L` enters live-send (focus right)

The existing `h`/`l` arms handled bare group collapse/expand navigation.
They were guarded to ignore Ctrl, and a new `Ctrl+L` arm was inserted that
calls `start_live_send()`, the same function `Tab` uses.

```rust
// BEFORE
KeyCode::Left | KeyCode::Char('h') => { /* group collapse */ }
KeyCode::Right | KeyCode::Char('l') => { /* group expand */ }

// AFTER
KeyCode::Left | KeyCode::Char('h')
    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
{ /* group collapse, unchanged */ }

// NEW: Ctrl+L → enter live-send (focus terminal pane)
KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
    if let Some(id) = self.selected_session.as_deref() {
        if self.get_instance(id).is_some_and(|i| i.is_structured()) {
            return Some(Action::SetTransientStatus(
                "Structured sessions have no terminal pane; press Enter to open the structured view."
                    .to_string(),
            ));
        }
    }
    if let Some(action) = self.start_live_send() {
        return Some(action);
    }
}

KeyCode::Right | KeyCode::Char('l')
    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
{ /* group expand, unchanged */ }
```

#### Change B: `Ctrl+H` exits live-send (focus left)

Inside `handle_live_send_key`, a hardcoded `Ctrl+H` check was added just
before the user-configured exit chord list. This means `Ctrl+H` always exits
live-send regardless of what `live_send_exit_chord` is set to in config.

```rust
// NEW: added before the existing chord_list_matches check
if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::CONTROL) {
    self.exit_live_send_and_restore_sizing(&state);
    return;
}
// existing exit chord check follows unchanged
if live_send::chord_list_matches(&state.exit_chords, key) { ... }
```

**Result:**

| Key | Context | Action |
|-----|---------|--------|
| `Ctrl+L` | Session list (list focused) | Focus jumps to right terminal pane |
| `Ctrl+H` | Terminal pane (live-send active) | Focus jumps back to session list |
| bare `h` / `l` | Session list | Still collapses/expands groups (unchanged) |
| `Tab` / `Ctrl+Q` | Any | Original enter/exit live-send still works |

## 2. Fixed: Preview Selection Highlight Flickering

### Symptom

When text is selected in the right preview pane (the gray selection highlight),
the highlight flickers or blinks rapidly. It is most visible when the agent is
actively producing output while a selection is held.

### Root cause

PR #3011 correctly froze the captured text while a selection was visible, but
three presentation paths remained active over that frozen snapshot:

- Live-send continued placing the real blinking terminal cursor over the
  preview, including when it overlapped the selection.
- Highlighted cells retained ANSI reverse, blink, and hidden modifiers from
  the captured agent output. Those modifiers could override or flash through
  the selection colors.
- The live-send ticker and capture-worker notifications continued requesting
  frames even though the frozen preview had no new content to show.

### Fix

- `src/tui/home/render.rs` hides the live-preview cursor for the lifetime of a
  selection and removes only unstable modifiers from highlighted cells. Bold,
  italic, underline, and all ordinary agent styling remain intact.
- `src/tui/app.rs` ignores preview-only ticker, post-key, and capture-worker
  refresh signals while the snapshot is frozen. Direct mouse movement,
  scrolling, auto-scroll, status changes, and other real UI updates still draw.
- The earlier global reverse-video stripping in `parse_output_text` was
  removed. It changed legitimate agent UI outside the selection and did not
  address the independently blinking hardware cursor.

### Verification

- `cargo fmt --all -- --check`
- `cargo clippy --lib -- -D warnings`
- Preview drag-selection and frozen-preview regressions passed
- See the post-sync full-suite results in section 5

## 3. Fixed: Typed Characters and Styled Text Flashing

### Symptom

While typing in an interactive preview or chat/composer, a newly entered
character could appear for one frame and disappear on the next. Existing
styled transcript text, including strikethrough text, could flash at the same
time.

### Root cause

Two rendering paths could expose intermediate state:

- The synchronized-update opener and cursor hide were flushed before the
  frame contents, leaving a visible gap while the widgets were built.
- Shell terminal previews sampled the in-process VT grid while a prompt was
  repainting. A transient VT seed frame could briefly overwrite tmux's stable
  rendered cells.

### Fix

- `src/tui/app.rs` queues the synchronized-update opener and cursor hide with
  the draw payload, so the terminal receives one atomic frame.
- Terminal previews now render from tmux cell snapshots. Native agent and tool
  previews retain the low-latency VT path.
- A read-only OSC 52 observer preserves terminal clipboard forwarding without
  using the transient VT grid for rendering.
- The transport choice and stale-clipboard behavior have regression tests.

This is a compatibility backport of the relevant behavior from upstream fix
`#3315`, integrated with the local selection and focus changes.

### 3.1 Claude-specific repaint flicker

After the transport fix above, Claude's own classic main-screen renderer could
still repaint a freshly typed character or a styled transcript cell (especially
strikethrough) through an intermediate blank frame. That repaint happens inside
Claude before AoE captures the pane, so making AoE's outer frame atomic cannot
remove it.

`src/session/instance.rs` now adds this environment variable to every Claude
command launched by AoE, including default commands, custom commands, host
sessions, and sandboxed sessions:

```text
CLAUDE_CODE_NO_FLICKER=1
```

Claude 2.1.228 recognizes this as its flicker-free fullscreen renderer. It is
scoped to Claude processes launched by AoE; the global `~/.claude/settings.json`
file is not changed.

The setting is read when Claude starts. Existing panes therefore need to be
restarted once before this part of the fix is active. In the installed Claude
version, `/tui fullscreen` can also switch the current Claude process without a
restart.

### 3.2 Stable capture encoding across typing redraws

A later upstream report matches the remaining symptom more closely: while a
terminal application repaints during typing, the preview can alternate between
two captures that are semantically the same but encode full-width styled blank
cells differently. The visible result is a small background, modal, or line
shift rather than a whole-frame flash.

- [Issue #3336](https://github.com/agent-of-empires/agent-of-empires/issues/3336)
  identifies the mismatch between the live VT grid (which retains trailing
  background-styled blanks) and fallback `tmux capture-pane` frames (which trim
  them by default).
- [PR #3339](https://github.com/agent-of-empires/agent-of-empires/pull/3339)
  adds tmux's `-N` option to every preview-feeding capture so both transports
  preserve the same cells. The PR was still open and absent from `main` when
  this local patch was updated on 2026-08-13, so its two commits were reviewed
  and applied locally.
- `src/tmux/session.rs` now preserves trailing cells in single-pane,
  composited, and layout captures. `src/tmux/vt.rs` does the same for the VT
  seed snapshot. Status-detection and smart-rename captures remain unchanged.
- A real-tmux regression paints a complete 40-column background-styled row and
  verifies that every preview transport retains it.

### Verification

- `cargo fmt --all -- --check`
- `cargo clippy --lib -- -D warnings`
- Focused terminal-snapshot, OSC 52, atomic cursor, and selection regressions
  all passed
- Claude default-command and custom-command no-flicker launch regressions passed
- The real-tmux 40-column trailing-background regression passed
- See the post-sync full-suite results in section 5

## 4. Focused Left Pane Has a Consistent Outline

When focus is on the left `aoe` session list, all four perceived edges now use
the theme accent color at the same one-cell thickness, while the preview's
remaining outline blends into the theme background. In live-send the treatment
reverses: all four preview edges use the accent and the list outline blends into
the background. Titles, status icons, pane contents, and geometry remain
visible and unchanged; only the inactive box lines disappear.

The preview still owns the shared center seam. `src/tui/home/render.rs`
recolors the existing seam cells instead of rendering another block, preserving
the single-border layout, compact titles, and one-cell thickness with less
work.

A rendered-buffer regression verifies both focus directions and the color on
every edge, while the existing single-seam and compact-title tests verify that
the border remains one cell wide and no status context is lost.

## 5. Synced with Upstream `main`

The repository uses `main` as its default branch; it has no upstream `master`
branch. On 2026-08-13 the local base was fast-forwarded by 32 commits:

```text
86ccdd16 -> e7a35d75
```

That sync includes the v1.14.1 release and upstream versions of the terminal
snapshot / synchronized-frame work. The unique local behavior was then
reapplied and reconciled on top: `Ctrl+H` / `Ctrl+L`, stable selection styling,
Claude's flicker-free launch mode, the violet live chip, and the focused-list
accent outline. A named backup remains in `git stash` as a recovery snapshot.

On 2026-08-21 `main` was fast-forwarded by another 67 commits:

```text
e7a35d75 -> 65001957
```

This sync includes upstream status fixes, render-loop poller repair, trailing
background-fill capture parity, held-key and multiline live-send fixes, Codex
permission shortcuts, structured-view state improvements, and current
dependency updates. The 2026-08-21 local work was reapplied on top, with its
named pre-sync stash retained as a second recovery snapshot.

### Earlier 2026-08-13 verification

- `cargo fmt --all -- --check` passed.
- `cargo clippy --lib -- -D warnings` passed.
- All focused regressions passed, including the real-tmux trailing-background
  capture test and the Claude launch, selection, frozen-preview, and terminal
  snapshot tests.
- The complete library suite, excluding two known upstream-baseline failures,
  passed with 4,523 passed, 2 ignored, and 2 filtered out.
- The two excluded OMP container-capture tests fail identically in a pristine
  detached worktree at `origin/main` (`e7a35d75`), so they are not introduced
  by these local patches.
- `cargo build --release` completed successfully, and the linked executable
  reports `aoe 1.14.1`.

## 6. Local Build and Active `aoe` Command

The normal command is linked directly to this repository's release build:

```text
/opt/homebrew/bin/aoe -> /Users/jiaxux/agent-of-empires/target/release/aoe
```

Rebuild after changing the source:

```bash
cd ~/agent-of-empires
source "$HOME/.cargo/env"
cargo build --release
```

Because the command is a symlink, a successful release build immediately
becomes the version launched by `aoe`. Verify the active executable with:

```bash
command -v aoe
realpath "$(command -v aoe)"
aoe --version
```

The previous Homebrew link was:

```text
/opt/homebrew/bin/aoe -> ../Cellar/aoe/1.14.0/bin/aoe
```

Restore it if needed with:

```bash
ln -sfn ../Cellar/aoe/1.14.0/bin/aoe /opt/homebrew/bin/aoe
```

A later `brew upgrade` or `brew link --overwrite aoe` may restore Homebrew's
own link, in which case the repository link must be applied again.

Re-applying the patch after a future `git pull` requires checking that
`src/tui/home/input.rs` still contains the two changes above (git merges them
automatically if there are no conflicts, as happened during our session).

## 7. Kanagawa Wave Theme

A built-in `kanagawa-wave` theme now matches the palette already used by the
local Ghostty and tmux configuration. It uses Sumi Ink for the background,
Crystal and Spring Blue for titles and terminal focus, Oni Violet for active
focus and live mode, warm gold for attention and search, and Kanagawa's native
green/red family for statuses and diffs.

The theme is available from AoE's theme picker and through:

```toml
[theme]
name = "kanagawa-wave"
color_mode = "truecolor"
```

The existing one-cell focused-pane outline and Oni Violet live chip remain
unchanged structurally; the new palette makes them part of one cohesive visual
system rather than isolated accents.

The regular bottom bar also carries compact selected-session context in the
form `◆ Claude · Running · branch` (or the corresponding agent and state).
The branch segment drops first on narrower terminals, followed by state, so
essential shortcut hints remain visible. Live-send keeps its dedicated mode
banner instead of duplicating this context. The whole regular footer now uses
the calm theme background, matching the preview-focused live-send footer. It
no longer paints Kanagawa's blue `selection` swatch across the row. A rendered
buffer regression checks every footer cell for that unwanted strip and checks
that the identity and `Live` action share one palette.

## 8. Performance Audit

The local changes were reviewed specifically for work performed while typing
and on every frame:

- Frozen previews now discard preview-only ticker, post-key, and capture
  notifications. This reduces drawing while selecting or reading scrollback.
- Common agent labels such as Claude and Codex use borrowed static strings, and
  the regular footer's static key and description labels no longer allocate a
  new `String` on every frame. Unknown custom agent names retain the existing
  title-casing behavior.
- The theme is parsed when it is loaded, the Claude environment override is
  assembled only when a process launches, and `Ctrl+H` / `Ctrl+L` add only
  constant-time input checks. None adds polling, disk I/O, or subprocesses to
  the render path.
- Preview capture remains on the existing background worker. The `-N` flag
  preserves cells in the same tmux calls rather than adding captures. On an
  isolated 200 by 60 tmux pane with a full-width styled background, 300
  captures took 1.14 seconds without `-N` and 1.16 seconds with it, about
  0.07 milliseconds of measured overhead per capture. The live interval is
  25 milliseconds.
- Selection styling visits only the visible selected cells. The focused pane
  seam recolors the existing cells in one terminal column (side by side) or one
  terminal row (stacked), avoiding a second widget render. Both paths are
  bounded by the visible viewport and inactive when their feature is not shown.

Focused performance-sensitive regressions cover frozen refresh suppression,
stable selection modifiers and cursor handling, terminal snapshot routing,
trailing-cell preservation, footer rendering, focus keys, and pane borders.

## 9. Idle CPU and Poller Repair

Session-ID poller repair no longer runs in the main event loop on every wake.
It now shares the existing 500 millisecond status-maintenance interval, and
structured sessions are skipped because they do not own a tmux terminal pane.
This removes repeated tmux probes and unnecessary poller work from the hot
path while preserving automatic repair for terminal sessions.

The repair regression verifies both sides of the change: a stopped terminal
poller is replaced during scheduled maintenance, while a structured session's
poller is left untouched.

## 10. Dialogue Typing and Mouse-Scroll Responsiveness

Local live-send, dialogue, and embedded-composer input no longer waits for the
5 millisecond unmarked-paste look-ahead on every printable key. Those surfaces
already own text input, and ordinary local clipboard pastes arrive as a
bracketed `Event::Paste`, so characters can be dispatched immediately. The
look-ahead remains enabled on the home list, where pasted letters could invoke
shortcuts, and under Mosh, which strips bracketed-paste markers.

Mouse-wheel input forwarded into a full-screen agent now clears any preview
selection before the inner application scrolls. A selection deliberately
freezes its snapshot; keeping it while the agent replaced the underlying cells
could therefore leave the dialogue pane looking permanently stuck. Ordinary
AoE capture-window scrolling still preserves its scrollback-anchored selection.

Live forwarded wheel events also avoid an immediate redraw of the stale
pre-scroll snapshot. The capture worker's change notification paints the fresh
agent frame instead, preventing high-resolution trackpad bursts from queuing
redundant frames. State transitions such as entering local scrollback, returning
to the live edge, or clearing a selection continue to draw immediately.

Focused regressions cover the local-versus-Mosh paste policy, stale-frame draw
gating, selection clearing for forwarded inner scrolls, local selection
preservation, and frozen-preview refresh behavior. Final verification passed
formatting, strict Clippy (`-D warnings`), the optimized release build, and the
complete library suite with 4,526 passed, 2 ignored, and the 2 documented
upstream-baseline OMP container tests filtered out.

## 11. Atomic Codex Repaints

The live VT preview now honors DEC synchronized-update boundaries emitted by
terminal applications such as Codex (`CSI ? 2026 h` / `CSI ? 2026 l`). AoE
continues feeding every raw byte directly into its existing VT parser, but it
withholds preview sampling and wakeups while an explicitly synchronized frame
is incomplete. The closing marker publishes the fully parsed frame once, so a
separator row or typed text can no longer disappear during Codex's internal
clear-then-repaint sequence.

The boundary recognizer carries only a prefix length and a boolean across
socket reads. It performs no buffering, allocation, subprocess, or additional
polling, and the input-dispatch path is unchanged. Ordinary output still wakes
the preview immediately. A completed synchronized frame is classified as
authoritatively stable and bypasses the older 6 millisecond heuristic debounce,
so the fix does not add a fixed delay to typing echo and reduces redundant
intermediate render work.

Malformed applications cannot hold the preview indefinitely: an unmatched
begin marker fails open on the socket's existing read timeout. Regressions
cover ordinary immediate output, complete single-read frames, begin/end markers
split across reads, near-matches, fail-open behavior, both preview samplers,
suppressed intermediate wakeups, and immediate publication on close. Final
verification passed formatting, strict Clippy (`-D warnings`), all 45 VT tests,
all 61 live-send tests, and the complete library suite with 4,533 passed, 2
ignored, and the 2 documented upstream-baseline OMP container tests filtered
out.

The colored live-send badge now names the actual destination instead of showing
the generic `LIVE ->` copy. Agent panes display identities such as `CODEX` or
`CLAUDE`; terminal, container, and named tool panes display their corresponding
target. The value comes entirely from session metadata already held in memory,
with no pane query, polling, or change to the typing path.

The destination label shares the footer's normal background instead of using a
separate highlight fill. Only its foreground uses the active theme accent, so
the entire bottom row remains one continuous surface.

The AoE list and Preview pane outlines now use heavy box-drawing glyphs. They
remain exactly one terminal cell wide, preserving the existing content area,
single shared seam, focus masking, and render cost while making the selected
pane easier to identify.

## 12. Session Context and Agent-to-Agent Sharing

The selected session's recent context can now be inspected with `y` (`Y` in
strict-hotkey mode). The viewer uses Vim navigation, `s` opens a target picker,
and `Enter` explicitly confirms a one-way share to another session. `Esc` backs
out at every layer. The same action is discoverable from `Ctrl+K` and from the
session right-click menu.

Structured sessions read their recorded ACP events. Host Claude and Codex
sessions prefer their native transcripts, while other terminal and sandbox
sessions fall back to bounded tmux pane history. The viewer keeps at most 24
KiB and the outgoing prompt at most 16 KiB. Every share includes provenance
and labels the content as potentially stale background information rather than
new user instructions.

The feature is deliberately lazy. It creates its single sleeping background
worker only after context is requested, and all transcript reads, SQLite
queries, tmux captures, pane startup, and sends stay off the UI thread. There
is no new polling or render-time I/O. When no request is active, the event loop
skips the worker channel behind one integer guard. A regression specifically
verifies that `y` inside live-send is forwarded as ordinary agent input and
never opens the context viewer.

Full usage and source behavior are documented in
[`docs/guides/session-context.md`](docs/guides/session-context.md).

Final verification passed formatting, strict Clippy with `serve` enabled,
native Claude and Codex transcript-layout tests, context-menu and Vim-key
regressions, and the complete library suite with 4,540 passed, 2 ignored, and
the 2 documented upstream-baseline OMP container tests filtered out. The
optimized `cargo build --release --features serve` completed successfully and
updated the release binary used by the normal `aoe` command.

## 13. Event-Driven Rendering and Typing Latency

A process profile found two remaining sources of avoidable work on the TUI
thread. AoE woke the event loop every 33 milliseconds even outside live mode,
and every keyboard event synchronously recreated a directory and rewrote the
TUI activity marker.

The preview is now change-driven. Content, cursor-only movement, and clipboard
events wake the renderer directly. The existing 15 millisecond post-key wake
remains as a typing-echo fallback, but a real preview update cancels it so one
echo does not build the same frame twice. Preview drag-selection keeps its
33 millisecond ticker for smooth edge auto-scroll; ordinary maintenance and
frozen previews use a 120 millisecond cadence.

The activity marker is now refreshed at most once every five seconds. Push
notification suppression still uses its existing 30 second freshness window,
so this removes per-character filesystem I/O with a large safety margin and no
change in visible behavior.

An isolated optimized-release benchmark measured approximately 0.1 to 0.2%
CPU for an empty idle TUI and 1.1 to 1.3% while live-send watched an idle shell.
A 32-character injected burst appeared intact in the next captured frame. The
temporary tmux session, scratch directory, and isolated app data were removed
after the benchmark.

Verification passed formatting, production-target Clippy with warnings denied,
the app and live-send regression groups, and the full `serve` test suite with
only the two documented OMP environment cases explicitly skipped. The release
binary linked by the normal `aoe` command contains these changes.

## 14. Per-Pane Preview Scroll Memory

Switching to another session no longer discards the preview scroll position.
AoE remembers the nonzero offset for each session and restores it when that
session is selected again. Agent, host terminal, container terminal, and named
tool previews keep independent positions, so changing the displayed view and
returning also restores the exact pane the user was reading.

The state is process-local and remains bounded to visited panes with active
sessions. Returning a pane to the live edge removes its saved entry, deleted
sessions are pruned during reload, and switching profiles clears the map with
the other preview state. Restored offsets continue through the existing render
clamp if the available history became shorter while the pane was hidden.

This adds no timer, polling, capture, disk access, or render-time lookup. The
map is touched only on a session or preview-view transition, leaving scrolling
and typing hot paths unchanged. Regressions cover independent session offsets,
live-edge replacement, and agent-to-terminal view restoration.

## 15. Supervised Agent Dialogue

Two sessions can now exchange bounded findings under direct user supervision.
Open a session's context with `y` (`Y` in strict mode), press `c`, choose the
peer with `j` / `k`, and enter an objective. The same action is available in
the command palette and session right-click menu.

The relay prompts one participant at a time and advances only after AoE's
existing status path observes that turn finish. It captures the newest native
assistant turn on the background memory worker, sends only that bounded reply
to the peer, and rejects stale or duplicate worker results with dialogue and
operation ids. Transcript-record fingerprints distinguish a genuinely new turn
even when an agent repeats the exact same reply text. Recorded ACP events and
native host Claude or Codex transcripts are supported. Pane history remains
read-only and one-way because it has no reliable reply boundary.

The viewer uses Vim controls: `j` / `k` and `g` / `G` navigate, `p` pauses or
resumes, `x` stops, `+` adds two replies, and `Esc` closes the viewer without
discarding the dialogue. Manual live-send or message composition pauses the
relay for that participant, as do permission, stopped, and error states.

The supervisor has hard safety and token limits: six replies by default, 20 at
most, 8 KiB per reply, 64 KiB across replies, a 4 KiB objective, and a 30
minute deadline. Peer text is wrapped as untrusted content and cannot change
the relay controls or budget.

Performance remains event-driven. The feature adds no timer, polling loop,
per-keystroke parsing, foreground transcript read, or render-time I/O. It
reuses the existing lazy background worker, skips its channel when no result
is pending, and caches viewer wrapping by dialogue revision and width.

Final verification passed formatting, strict Clippy with `serve` enabled, the
focused dialogue, transcript, menu, binding, and typing-path regressions, and
the full production-feature library suite with 6,195 passed, 2 ignored, and
the 2 documented upstream-baseline OMP container tests filtered out. The
optimized `cargo build --release --features serve` completed successfully, and
`/opt/homebrew/bin/aoe` resolves to that newly built binary.

## 16. Completed Codex Confirmations Enter Waiting

Codex's `Stop` hook reports `Idle` for every completed response, including a
response whose final sentence explicitly asks for a binary decision such as
`Proceed with the two paired jobs under this fully documented contract? y/n`.
AoE previously trusted that hook without reading the completed response, so
the row could remain idle-colored instead of using the theme's yellow Waiting
color.

AoE now performs one bounded pane reconciliation for each newly written Codex
Idle hook. It recognizes an explicit `y/n`, `yes/no`, or equivalent
binary-answer suffix only when it is the latest response tail immediately
above Codex's newest `Worked for` divider and the current input prompt is
parked. Older questions, quoted prose, diff content, and a question that has
already been answered do not match.

The detector also recognizes Codex's current `»` prompt glyph. Each result is
cached against the hook file's modification time and carried through both the
TUI and daemon pollers. An unchanged unanswered question therefore does not
spawn another `tmux capture-pane`; a later hook write invalidates the cache and
is evaluated once. The work stays inside the existing background status
poller, so there is no render-time I/O, per-keystroke parsing, new timer, or new
polling loop. Focused unit and real-tmux pipeline regressions cover the exact
reported pane, an equivalent `yes or no` directive, stale prompt text, the
same-generation no-recapture contract, and the existing Claude approval
reconciliation.

Verification on the 2026-08-21 merged tree passed `cargo fmt`,
`git diff --check`, production-library Clippy with warnings denied,
`cargo check --all-targets --features serve`, all 6,384 library tests and every
integration/doc-test binary under `cargo test --features serve`, plus the
focused real-tmux and TUI/daemon cache propagation regressions. The optimized
`serve` release build completed, and `/opt/homebrew/bin/aoe` resolves to that
exact binary.

## 17. Trash Closes After a Successful Restore

Restoring a session now collapses the Trash section and selects the recovered
session in its active group. The shared restore action covers the `z` shortcut,
the context-menu action, and Restore All. A failed restore leaves Trash open so
the affected row remains visible alongside the error.

The change is a single in-memory state update on the successful restore path.
It adds no polling, disk access, rendering work, or typing-path overhead.

## 18. Waiting Prompts Color the Focused Pane

This experiment was superseded by section 21. Waiting prompts retain their
row-level status color, but no longer recolor the focused pane outline.

Codex command approvals such as `Would you like to run the following command?`
were already detected as Waiting, but only the small session-row spinner and
text used the theme's Waiting color. The large focused outline kept its normal
accent, making the state easy to miss.

The focused AoE or Preview pane now uses the selected session's semantic
Waiting color for its complete thick outline and title. The other pane remains
visually hidden, preserving the one-focus-box interaction. Kanagawa Wave uses
its existing orange `#ff9e3b`. This is a constant-time in-memory status lookup
during an existing render and adds no capture, polling, I/O, timer, layout
change, or typing-path work.

## 19. Codex Always-Allow Uses the Current Prompt Shortcut

The sidebar permission dialog still opens with `a` in normal mode and `A` in
strict mode. Inside the dialog, `Shift+A` still selects Allow Always. AoE now
sends Codex the `p` shortcut advertised by its current command-approval prompt
instead of the obsolete `a`, so the request is actually accepted.

This changes one static agent-response token. It adds no input handling,
polling, rendering, or typing-path work.

## 20. Upstream 1.15 Integration

The customized tree now follows upstream `main` through commit `ec6b6d04`,
including release 1.15.0 and the two newer ACP diagnostic fixes. The sync adds
atomic session creation, unique rename enforcement, tmux rekeying after title
persistence, bounded git and container control commands, regular-file checks
for container transcript discovery, closed-by-default web plugin panes, and
updated ACP adapter compatibility warnings.

All local preview, Kanagawa, keyboard, memory, dialogue, status, scrolling,
trash, and performance changes were reapplied after the fast-forward. The two
merge intersections were reconciled without adding a timer, polling loop,
render-time I/O, or typing-path work.

The active `/opt/homebrew/bin/aoe` symlink now resolves to the optimized
1.15.0 `serve` build. AoE's official `update` command deliberately does not
replace this source-linked custom binary with a release tarball because doing
so would discard the local features. A recoverable pre-sync stash remains as
`stash@{0}`.

## 21. Stable Focus Outline Color

Waiting sessions no longer change the focused AoE or Preview outline to the
theme's orange Waiting color. The focused outline keeps its normal accent,
which is Oni Violet in Kanagawa Wave, while the inactive pane outline remains
hidden as before. Waiting is still visible through the session row's spinner,
text, and semantic status color.

The render path is simpler than before: it no longer looks up the selected
session status to choose an outline color. This adds no polling, I/O, timer,
allocation, or typing-path work.

## 22. Scrollback Returns to Live Output on Input

Scrolling up still freezes the current preview snapshot so history remains
stable while it is being read. Sending a typed key, bracketed paste, or a
message from the composer now returns the displayed target to the live edge,
so the resulting echo cannot remain hidden behind that frozen snapshot and
make the page look stuck.

The reset occurs only after input is actually routed to the displayed pane.
Navigation keys reserved for reading scrollback keep their existing behavior,
and per-pane scroll memory remains intact until interaction resumes. The common
typing path at the live edge adds only one predictable integer comparison. It
adds no capture, polling, timer, render-time I/O, allocation, or subprocess.

## 23. Fleet Checkpoint Capture

Fleet Operations now captures durable checkpoints for the session selected
when the view opens. In the Checkpoints panel, `c` opens an inline label editor
and `Enter` queues capture. The row publishes `capturing` immediately, and
printable navigation letters remain ordinary text while the editor owns input.

A lazy, dedicated worker writes versioned manifests atomically and stores Git
patches plus safe untracked-file archives by SHA-256 content hash. Common secret
and credential paths are excluded. Redacted, unstable, oversized, or otherwise
partial captures are labeled incomplete with a concrete reason instead of
being presented as restorable. Durable history loads only when the Checkpoints
panel is first visited and survives source-session removal.

Git and filesystem work remains outside input and rendering. The worker does
not exist until checkpoint history or capture is requested, and capture never
pauses or mutates the running agent. The lifecycle operations added below use
the same dedicated worker.

## 24. Safe Historical Checkpoint Forks

Finished Fleet checkpoints can now create a new ordinary AoE session with `f`.
The checkpoint worker verifies the manifest, exact commit, artifact type,
length, media type, and SHA-256 hash before the normal session builder creates
an isolated AoE-owned worktree. Materialization applies the tracked binary
patch and safe untracked archive under bounded path, file-count, and byte
limits. It refuses symlink traversal, dirty or mismatched targets,
multi-repository snapshots, and excluded secret paths.

The child records checkpoint provenance and starts a fresh conversation. The
source session remains untouched. Any failure cleans only the newly created
worktree, including a dirty partially materialized checkout. Git work and
artifact reads remain on background workers.

## 25. Background Agent Trials

Pressing `t` on a finished checkpoint now opens a paste-aware prompt editor and
a keyboard-first contender picker. Two through eight contenders are persisted
in a versioned Trial manifest and created as ordinary terminal sessions from
the same verified checkpoint. A single installed agent can be selected twice
for an A/B run.

Contender setup uses the existing session builder, trusted hook policy,
checkpoint cleanup, and detached restart/prompt path. It never auto-attaches or
moves dashboard selection. Admission is reserved through the existing fleet
governor before creation and converted to a real session lease in the same UI
turn, so the configured worker ceiling is not oversubscribed. Trial rows show
each agent's live state, elapsed time, token usage when reported, and specific
failures. Terminal state is monotonic on disk, and graceful TUI shutdown stops
pending launches without deleting already-created sessions.

The launcher deliberately uses fresh terminal conversations. Each Trial now
requires a bounded repository verification command. Results, duration, retry
history, reported tokens when available, deterministic ranking, cancellation,
and a retained-winner marker persist across AoE restarts. `y` retries an
eligible failed/cancelled contender, `x` confirms cancellation of an active
Trial, and `w` retains a completed winner without deleting other sessions.

## 26. Complete Checkpoint Lifecycle

Checkpoint `i`, `e`, `d`, and `r` now inspect, export, delete, and restore.
Exports verify manifests and content hashes before producing a portable bundle.
Deletion is confirmed and reference-counted, so shared artifacts survive until
their last manifest is removed.

Restore accepts only a stable one-repository source and a clean target. A
background preflight verifies the checkpoint in a disposable worktree, then
previews HEAD movement, changed-path count, and a recovery ref. Confirmation
revalidates the target and preserves that ref before mutation. Corrupt,
redacted, incomplete, dirty, incompatible, and multi-repository cases are
refused without changing the source.

## 27. Durable Campaign Operations

The Campaign panel now creates durable tasks with `n`, supports keyboard
selection of blocking dependencies, rejects cycles and duplicate producer
requests, and advances an incremental ready frontier. `a`, `w`, `v`, `m`, `f`,
and `x` record active, waiting, review, done, failed, and cancelled states on a
lazy storage worker. A ready task linked to the selected session enters
Attention. Campaign transitions remain operator-controlled and do not send
prompts autonomously.

## 28. Fleet Attention, Conflict, and Readiness Facts

Attention now combines status/unread facts with failed Trial verification,
worker-capacity refusal, repository conflicts, and linked ready campaign tasks.
`a` persists acknowledgement of exactly one occurrence by opaque SHA-256 ID;
a new revision reappears, and a failed write rolls the optimistic UI change
back.

Conflict results are generation-stamped and repository-scoped. `r` explicitly
invalidates cached scans, while a completed work cycle invalidates exactly once.
`s` jumps to a deterministic conflict owner and opens AoE's existing message
composer. Conflict and verification facts also feed readiness. Missing tests,
format, lint, review, token, and cost producers stay visibly unknown instead of
being invented.

## 29. Shared CLI, HTTP, and Lazy Web Fleet View

`aoe fleet` prints a compact report and `aoe fleet --json` emits the complete
schema-versioned projection. `GET /api/fleet` produces the same report on a
blocking worker. CityHall returns `403` and hides the web entry point.

The web dashboard lazy loads its read-only Fleet modal and makes no Fleet API
request until the operator opens it. `h`/`l`, arrows, `j`/`k`, `Enter`, `/`, and
`Esc` are locally scoped while ordinary browser `Tab` behavior remains intact.

## 30. Fleet Performance Regression Report

An ignored release-mode test reports p50/p95 distributions for unchanged
session folds, complete rebuilds, and immutable snapshot reads at 1, 20, 100,
and 500 sessions. On the 2026-08-27 development machine, the 100-session p95
was 32.625 microseconds for unchanged sync plus snapshot, 94.625 microseconds
for a full rebuild, and at most 0.042 microseconds for a snapshot read. Normal
CI retains deterministic structural checks for dormancy, coalescing, and stale
result rejection rather than enforcing hardware-dependent elapsed times.

## 31. External Approval Autopilot

An opt-in `[session] approval_autopilot` setting now lets AoE answer verified
terminal tool-permission prompts. Complete known persistent choices are
preferred (`a` for Codex file edits, `p` for Codex commands, and the mapped
Claude choice), with allow once as the fallback for two-choice menus and OMP.
Codex persistent-selection shortcuts are followed by `Enter` so the highlighted
choice is submitted rather than merely moved.
The strict detector covers Claude Code, Codex command and file-edit approvals,
and OMP while excluding ordinary chat confirmations, `AskUserQuestion`, plan
questions, and folder-trust gates. Codex command approvals recognize both
stock two-choice and three-choice menus, including wrapped option labels.
Unknown prompt shapes remain manual.

The TUI reuses the status poller's existing pane capture and cached config, and
the daemon mirrors config changes through its file watcher. A per-session
cross-process lock plus a hashed, time-bounded prompt marker prevents a TUI and
daemon from dispatching the same approval twice. Exact repeats remain
suppressed, while distinct verified prompts can be approved back-to-back
without an arbitrary delay. No prompt text is persisted, and the disabled path
adds no pane capture, file lock, or config read. Enabled prompt verification
stays on the existing background status workers and does not enter rendering,
key translation, live-send batching, or paste handling.

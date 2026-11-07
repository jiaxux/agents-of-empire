# Fleet Operations

Fleet Operations is AoE's keyboard-first control plane across active agent
sessions. It combines attention, repository conflicts, readiness, capacity,
campaigns, trials, and durable checkpoint history without adding work to the
normal typing or render paths.

## Open Fleet Operations

| Surface | Entry point |
| --- | --- |
| TUI | Select a session and press `Ctrl+F` |
| CLI | Run `aoe fleet` or `aoe fleet --json` |
| Web dashboard | Choose Fleet Operations in the dashboard or top bar |
| HTTP | Request `GET /api/fleet` |

The TUI starts its Git and persistence workers lazily. The web dashboard lazy
loads the modal and does not request `/api/fleet` until the modal opens. The
HTTP handler builds its one-shot report on a blocking worker rather than the
async executor. Fleet Operations is unavailable in CityHall mode, where the
web entry point is hidden and the endpoint returns `403`.

## Navigate the TUI

| Key | Action |
| --- | --- |
| `Tab`, `l`, `Right` | Next panel |
| `Shift+Tab`, `h`, `Left` | Previous panel |
| `j`, `k`, arrows | Move between rows |
| `g`, `Home` | First row |
| `G`, `End` | Last row |
| `Enter` | Open the row's owning session when it has one |
| `/` | Filter the current panel |
| `Esc` | Leave an editor, clear a filter, or close the view |

Editors own printable input, including letters that are Fleet shortcuts. An
applied filter is cleared before `Esc` closes the view. Closing Fleet restores
the dashboard without attaching to or moving any unrelated session.

## Attention

Attention ranks real session failures, approval or input waits, unread output,
capacity refusals, conflicts, failed Trial verification, and newly ready linked
campaign tasks. Only one highest-priority occurrence is shown per session.

Press `a` to acknowledge the selected occurrence. Acknowledgements are stored
durably by opaque SHA-256 item ID and survive an AoE restart. If the underlying
fact changes revision, the new occurrence receives a new ID and becomes visible
again. A failed persistence write rolls back the optimistic acknowledgement and
keeps the item visible.

## Conflicts and coordination

AoE scans changed paths on a bounded background worker and groups overlaps only
between sessions that share a repository identity. Identically named files in
unrelated repositories do not collide. Results carry a session generation, so
a late scan cannot restore stale conflicts.

| Key | Action |
| --- | --- |
| `r` | Invalidate and refresh conflict facts |
| `s` | Jump to one conflict owner and open the existing message composer |
| `Enter` | Jump to the first stable conflict owner |

A work cycle that moves from active to stable invalidates its old scan once.
Stable, unchanged sessions do not keep spawning Git commands.

## Readiness and capacity

Readiness is evidence, not a guessed green light. The initial policy requires:

- a current repository diff result;
- a current conflict result;
- a current verification result, currently produced by a linked Agent Trial.

Failed or missing required evidence produces an explicit blocked or missing
verdict. Tests, format, lint, and review are also listed as missing until a real
producer supplies them. AoE does not invent CI, review, token, or cost data.

The Budget panel shows live worker usage against
`acp.max_concurrent_workers`. Trial launches and retries reserve capacity before
session creation and release it on every terminal or failure path. A rejected
active session becomes an Attention item. Reported token usage remains
`unknown` when the selected agent adapter does not supply it.

## Checkpoints

The Checkpoints panel targets the session that was selected when Fleet opened.

| Key | Action |
| --- | --- |
| `c` | Capture with an optional label |
| `i` | Inspect the versioned manifest |
| `e` | Export a verified portable bundle |
| `d` | Delete after confirmation |
| `r` | Restore in place after preflight and confirmation |
| `f` | Fork a new session at the historical code state |
| `t` | Start an Agent Trial from the checkpoint |

### Capture and storage

Capture publishes a `capturing` row immediately and runs on a dedicated lazy
worker. A manifest records session and conversation identity when known,
agent/model/effort/mode, safe sandbox metadata, repository HEAD and base
commits, captured verification evidence, and content hashes.

Tracked changes are stored as a binary Git patch. Safe untracked regular files
are stored in a deterministic archive. Artifacts are content-addressed, so
identical bytes are reused. Common credential paths such as `.env`, `.ssh`,
private keys, and plugin payload directories are excluded.

Redacted, unstable, oversized, non-Git, and multi-repository captures are
marked incomplete with a concrete reason. AoE never describes a partial
capture as exactly restorable.

### Inspect, export, and delete

Inspect reads metadata only. Export verifies the manifest and every referenced
artifact before writing a versioned bundle to AoE's checkpoint export
directory. The checkpoint store can also import such a bundle through its
domain API.

Delete is confirmed and reference-counted. The manifest is removed, but shared
content remains until its last checkpoint reference is deleted.

### Fork and restore

Fork verifies repository identity, commit availability, artifact media type,
length, and SHA-256 hash before creating an ordinary AoE-owned worktree through
the normal session builder. The source session is unchanged. The child records
checkpoint provenance and starts a fresh agent conversation at the historical
code state.

Restore is intentionally stricter. The source session must be idle, stopped,
or in error, the target must be one repository and clean, and the checkpoint
must pass a disposable-worktree replay. AoE then previews the old and new HEAD,
changed-path count, and recovery ref. After confirmation, the worker revalidates
the target and preserves the recovery ref before mutating it. Multi-repository,
redacted, corrupt, missing, or incompatible checkpoints are refused.

## Agent Trials

On a complete checkpoint, press `t`:

1. Enter the task prompt.
2. Enter a repository verification command, such as `cargo test`.
3. Select between two and eight contenders with `j`/`k` and `Space`.
4. Press `Enter` to launch.

If only one compatible agent is installed, AoE offers two independent
contenders for an A/B run. Each contender is an ordinary terminal session in a
separate worktree materialized from the same verified checkpoint. Trusted
repository hooks and profile hooks follow the normal builder policy. The
launcher does not attach automatically or move dashboard selection.

When a contender reaches a stable terminal state, AoE runs the bounded
verification command in that contender's worktree. Trial manifests persist
prompt, recipe, provenance, retry history, elapsed time, reported tokens when
available, verification result, and a deterministic ranking. AoE recovers
unfinished Trial supervision after its own restart.

| Key | Eligibility and action |
| --- | --- |
| `y` | Retry a failed or cancelled contender from the same checkpoint |
| `x` | Confirm cancellation of a launching or running Trial |
| `w` | Mark a completed contender as the retained winner |

Cancellation stops pending orchestration but preserves already created
sessions and worktrees. Retaining a winner records the decision; it does not
silently delete the other contenders. Missing token usage remains unknown and
is never treated as zero.

## Campaigns

Campaign tasks are versioned, atomic, and durable. The default campaign is
created lazily when its first task is added.

1. Press `n` in Campaign.
2. Enter a task title.
3. If tasks already exist, select blocking dependencies with `j`/`k` and
   `Space`.
4. Press `Enter` to create it.

AoE rejects dependency cycles and duplicate producer requests. A planned task
becomes ready when all blocking dependencies are done. If it is linked to the
session selected when Fleet opened, that newly ready task enters Attention.

| Key | New task state |
| --- | --- |
| `a` | Active |
| `w` | Waiting |
| `v` | Review |
| `m` | Done |
| `f` | Failed |
| `x` | Cancelled |

Task state is operator-controlled. Fleet does not autonomously send prompts or
start agents from a campaign transition.

## Web, CLI, and HTTP projections

The TUI renders its immutable in-memory snapshot. CLI and HTTP consumers build
the same schema-versioned projection from current session state and the durable
checkpoint, Trial, campaign, and acknowledgement stores.

```bash
aoe fleet
aoe fleet --json
```

The text command prints a compact summary plus Attention and conflict rows.
JSON and `GET /api/fleet` expose the complete versioned report. The web modal is
read-only and supports `h`/`l`, arrows, `j`/`k`, `Enter`, `/`, and `Esc`. Normal
browser `Tab` behavior is preserved.

## Performance contract

Fleet rendering reads cached rows keyed by snapshot version, panel, and filter.
No render or ordinary key handler performs Git, filesystem, tmux, network, or
process work. Workers and durable history remain dormant until Fleet or the
relevant panel is opened. Repeated identical session inputs publish no new
snapshot, completed work invalidates exactly one Git scan, and stale worker
results are discarded.

Run the ignored release-mode distribution report when changing this path:

```bash
cargo test --release --lib fleet_state_performance_report -- --ignored --nocapture
```

The report covers unchanged sync, full rebuild, and immutable snapshot reads at
1, 20, 100, and 500 sessions without imposing hardware-dependent timing
assertions on normal CI.

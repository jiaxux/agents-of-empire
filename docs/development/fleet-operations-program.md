# Fleet Operations Program

Status: accepted for implementation

This program adds seven related capabilities to Agent of Empires without putting
Git work, transcript scans, network calls, or cross-session aggregation on an
input or render path:

1. worktree conflict radar;
2. fleet attention inbox;
3. checkpoints and historical forks;
4. agent Trials;
5. ship-readiness evidence;
6. budget and capacity governance;
7. campaign and task dependencies.

The features ship independently. The shared substrate ships first and remains
useful even when every user-facing feature is disabled.

## Product contract

AoE remains a fast session manager and operator control plane. It does not turn
into an autonomous project manager, a mandatory cloud service, or a replacement
for Git and issue trackers.

The following are release gates, not aspirations:

- Every mouse action has a keyboard path.
- Existing bindings keep their meaning. New global bindings are registered in
  the central action/keymap system and appear in contextual help.
- Focused editors, search fields, composers, live-send mode, and terminals keep
  ownership of text input. Fleet bindings do not leak through them.
- No Git subprocess, SQLite query, filesystem walk, transcript fold, plugin RPC,
  or network request runs from a TUI/web render function or keystroke handler.
- Disabling the fleet features has near-zero runtime cost: no pollers, no timers,
  no database, and no extra session-row work beyond a single option check.
- Partial data is normal. Missing ACP usage, GitHub, CI, or plugin information
  produces `unknown`, never a blocked UI or invented value.
- Every automatic mutation is opt-in and previewable. Observation features are
  enabled before automation features.

## Performance budgets

Benchmarks use synthetic fleets of 1, 20, 100, and 500 sessions. The 100-session
case is the normal regression gate; 500 sessions is the stress case.

| Path | Budget |
| --- | --- |
| TUI action dispatch | p95 below 2 ms, excluding terminal I/O |
| TUI frame contribution from fleet state | p95 below 1 ms at 100 sessions |
| Immutable fleet snapshot read | p95 below 250 us at 100 sessions |
| One-session index update | p95 below 2 ms when no Git refresh is required |
| Web fleet reducer update | p95 below 4 ms at 100 sessions on reference hardware |
| Idle background CPU | below 1% for 100 unchanged sessions |
| Idle wakeups | no more than one coalesced fleet tick per configured interval |
| Startup regression with features enabled | below 25 ms, excluding first Git scan |
| Startup regression with features disabled | below measurement noise |
| Web initial bundle | no eager feature bundle; route/panel chunks load lazily |

Performance tests report distributions rather than one elapsed assertion.
Timing assertions live only in the dedicated benchmark job so normal CI remains
deterministic. Unit and e2e tests assert structural safeguards: no work starts
from render, repeated identical inputs produce no update, expensive requests
coalesce, and cancellation drops obsolete results.

## Shared architecture

### The `fleet` module

`fleet` is a deep module. Its small external interface hides normalization,
incremental indexes, stale-result rejection, persistence, coalescing, and
feature-specific projections.

Conceptual interface:

```rust
pub struct FleetIndex { /* private */ }

impl FleetIndex {
    pub fn apply(&self, event: FleetEvent) -> ApplyOutcome;
    pub fn snapshot(&self) -> Arc<FleetSnapshot>;
    pub fn project(&self, query: FleetQuery) -> Arc<FleetProjection>;
}
```

The interface includes these invariants:

- `apply` is non-blocking with respect to Git, network, and durable storage.
- Snapshots are immutable and versioned.
- Applying the same source revision twice is idempotent.
- Results produced for an obsolete session generation are discarded.
- A slow consumer cannot block producers or another consumer.
- Ordering is causal per session and best-effort across independent sessions.
- Expensive enrichment is represented as a request and completed later by a
  bounded worker; it is never performed inside `apply`.

The module owns these internal indexes:

- session identity and generation;
- project/repository/worktree membership;
- changed-path and optional changed-hunk summaries;
- attention items and acknowledgement state;
- checkpoint metadata;
- trial membership and contender results;
- evidence facts and freshness;
- usage/capacity samples and policy decisions;
- campaign nodes and dependency edges.

### Events, snapshots, and adapters

Inputs arrive as typed `FleetEvent` values. Producers include session lifecycle,
status pollers, file watchers, bounded Git workers, ACP usage events, and plugin
state. Each producer is an adapter at a real seam because production and test
adapters both exist.

TUI, web, CLI, HTTP, and plugin hosts consume the same snapshot schema. They may
filter and render it but must not reconstruct fleet truth independently.

TUI-only builds use an in-memory index. Serve builds may persist durable facts
through the existing protocol-agnostic event store. The external `fleet`
interface is identical in both builds; storage is an internal seam.

### Work scheduling

One bounded scheduler owns enrichment jobs:

- keys are `(session_id, generation, enrichment_kind)`;
- submitting the same key coalesces with the existing job;
- a newer generation cancels or invalidates the prior result;
- visible/selected sessions receive priority without starving the remainder;
- unchanged repositories use cached object IDs and filesystem revisions;
- concurrency is configurable and defaults conservatively for laptops;
- every job has a deadline and cancellation token.

There is no independent timer per session.

### Persistence

Durable records are versioned and additive. The initial schema stores only
metadata and compact facts, not duplicate transcripts or full patches.

Large artifacts live in the existing per-session artifact directory and are
referenced by content hash. Deleting a session follows existing trash and purge
semantics; shared content is removed only after the last reference disappears.

## 1. Worktree conflict radar

### User outcome

The operator sees likely collisions while parallel agents are still working,
not after both branches are presented for merge.

### First release

- Index changed paths per repository and session.
- Compare only sessions with the same repository identity and compatible base
  ancestry.
- Classify overlap as `path`, `hunk`, `base_drift`, or `unknown`.
- Add a compact collision marker to session rows.
- Provide a keyboard-opened conflict list with `j`/`k`, arrows, `Enter`, and
  `Esc`; `/` filters without leaking characters to global actions.
- Expose read-only CLI and HTTP projections.
- Allow sending a coordination message through the existing send path.

### Later release

- Hunk overlap from zero-context diffs.
- Rename-aware matching.
- Suggested merge order based on ancestry and dependency metadata.
- Explicit, separately confirmed rebase/cherry-pick actions.

### Performance strategy

- File watching marks a repository dirty; it does not run Git.
- Refreshes debounce and coalesce by repository, not by session.
- The first pass stores sorted path hashes. Pairwise overlap uses an inverted
  `path -> small set of sessions` index instead of comparing every session pair.
- Hunk analysis runs only for overlapping paths and only when the panel is
  opened or a background budget is available.

### Acceptance criteria

- A file modified in two sibling worktrees produces one symmetric collision.
- Identical filenames in different repositories do not collide.
- Deleted, renamed, archived, and trashed sessions remove their index entries.
- A stale Git result cannot restore a collision for a newer generation.
- Five hundred clean sessions add no Git work after the initial snapshot.

## 2. Fleet attention inbox

### User outcome

One ordered queue answers: "What needs me next?"

### Item kinds

- approval requested;
- agent waiting for input;
- error or unhealthy worker;
- unresolved conflict;
- failed or blocked verification;
- CI/review blocker supplied by a plugin;
- exhausted budget or provider capacity;
- campaign dependency newly unblocked;
- user-defined reminder.

### Interaction

- A discoverable action opens the inbox from home and structured view.
- `j`/`k` and arrows move; `Enter` jumps to the owning session/action.
- `a` acknowledges only the selected item; bulk acknowledgement requires a
  separate confirmation.
- `/` filters; `Tab` cycles kind/project filters; `Esc` clears filter first,
  then closes.
- A repeatable next-attention action jumps without opening the full panel.
- Mouse selection and context actions call the same registered actions.

The exact defaults are chosen only after running the keymap collision and
legacy-terminal ambiguity audits.

### Ordering

Ordering is deterministic: explicit severity, actionable-now, age, project,
session title, stable item ID. Plugins may provide facts and suggested severity
but cannot silently outrank security confirmations or errors.

### Performance strategy

Items are updated incrementally from events. Rendering reads a pre-sorted
projection. Acknowledgement is O(1); filters operate on compact item metadata.

### Acceptance criteria

- Every item has a keyboard-only resolution path.
- Focused text input receives all printable keys.
- Acknowledgement survives restart when the underlying item identity survives.
- A resolved fact removes its item without requiring acknowledgement.
- Repeated events do not duplicate an item or reorder unrelated items.

## 3. Checkpoints and historical forks

### User outcome

The operator can name a safe point, inspect it later, fork from it when the
agent supports that operation, and export enough evidence to reproduce the
state without duplicating the whole repository.

### Checkpoint manifest

```text
schema version
checkpoint ID, label, timestamp
session ID and generation
repository identities, base commits, HEADs
working-tree patch/artifact content hashes
ACP event sequence or terminal conversation identity
agent, model, effort, mode, sandbox/runtime metadata
campaign/trial membership
verification and usage facts at capture time
```

Secrets, bearer tokens, credential paths, environment values, and unredacted
plugin payloads are excluded by default.

### Operations

- `create`: metadata capture is immediate; patch materialization is background.
- `inspect`: never starts an agent or mutates Git.
- `fork`: creates a new worktree/session, then uses the best supported
  conversation fork/resume mechanism; unsupported agents receive an explicit
  context primer rather than a fake historical fork.
- `restore`: always previews affected repositories and refuses a dirty target.
- `export`: creates a versioned bundle with a redaction report.
- `delete`: reference-counted artifact cleanup.

### Performance strategy

- Git object IDs and event sequence numbers are captured synchronously; patch
  bytes are generated later.
- Content-addressed artifacts deduplicate repeated checkpoints.
- Transcript history remains in its existing store and is referenced by range.

### Acceptance criteria

- Capturing a checkpoint never pauses a running agent.
- A completed manifest is internally consistent or marked incomplete with an
  actionable reason.
- Fork failure leaves the source untouched and cleans up only newly created,
  AoE-owned resources.
- Export/import round-trips manifest metadata across machines.

## 4. Agent Trials

### User outcome

One task can be attempted by multiple agents/models on the same starting state;
AoE presents comparable results and lets the operator keep a winner.

### Trial model

A `Trial` owns:

- immutable task prompt and starting checkpoint;
- contender specifications (agent, model, effort, permission mode);
- verification recipe and timeout;
- resource/budget policy;
- contender sessions and terminal results;
- optional human decision and retained winner.

### Workflow

1. Select a session/checkpoint and invoke Create Trial.
2. Choose two or more contenders with keyboard-navigable pickers.
3. Preview worktrees, commands, budgets, and permission modes.
4. Start contenders through the existing session builder.
5. Stream ordinary session state; no special terminal implementation.
6. Run the same repository-defined verification recipe for each contender.
7. Compare outcome facts: verification, changed files/lines, duration, usage,
   conflicts, evidence freshness, and human notes.
8. Retain one or more contenders; archive/delete remains an explicit choice.

AoE does not use an LLM judge by default. Deterministic verification and human
review outrank generated summaries.

### Performance strategy

- Trial state is metadata over ordinary sessions.
- Comparison projections update only when a contender fact changes.
- Diffs and logs load on demand; the summary table contains compact counters.
- Trial concurrency goes through the governor and existing worker ceilings.

### Acceptance criteria

- Every contender starts from the exact same checkpoint manifest.
- A partial launch can be retried without duplicating successful contenders.
- Cancellation never deletes user-owned worktrees.
- Missing usage or verification remains visibly unknown.
- Trial metadata does not alter normal session behavior.

## 5. Ship-readiness evidence

### User outcome

AoE shows what is actually known about readiness, with provenance and freshness,
instead of asking the agent to reconstruct it from memory.

### Evidence facts

- repository and diff summary;
- commits and base drift;
- commands/tests observed, exit status, duration, and working directory;
- configured verification recipe results;
- unresolved local review comments;
- conflict-radar status;
- PR, CI, review, and mergeability facts from the GitHub plugin;
- approvals and destructive operations;
- checkpoint/trial provenance;
- explicit agent claims, always labeled as claims rather than proof.

Every fact carries source, observed time, subject revision, and freshness. A
verdict is a pure projection over facts and policy. The initial feature does not
merge or push.

### Interface

Core exposes a small evidence-ingest interface for trusted built-ins and plugin
workers. The existing GitHub plugin remains the owner of GitHub transport and
authentication; core owns normalization and display policy.

### Performance strategy

- Facts are append/update events keyed by source and subject revision.
- Readiness is recomputed only for the affected session.
- Full command output remains an artifact; the index stores compact results.
- Plugin refresh cadence remains plugin-owned and rate-limit aware.

### Acceptance criteria

- Stale green CI for an older commit never marks the current HEAD ready.
- A command exit code without a recognized verification identity is evidence,
  not automatically a passed test suite.
- The generated PR handoff cites each claim's fact source.
- No network integration is required for a useful local readiness panel.

## 6. Budget and capacity governor

### User outcome

Operators can observe and constrain fleet resource use without AoE inventing
precision that an adapter does not provide.

### Data quality

Usage samples are `reported`, `estimated`, or `unknown`. Cost is calculated only
when model identity, units, and a versioned price entry are known. Estimates are
visually and structurally distinct from reported values.

### Policies

- warn at a per-session/project/profile threshold;
- stop launching new trial contenders above a fleet concurrency or budget cap;
- queue work while a provider is rate-limited;
- optionally pause at a safe prompt boundary;
- suggest model/provider alternatives;
- automatic model changes are off by default and require an explicit policy.

Existing sessions are never killed merely because telemetry disappeared or a
price table changed.

### Performance strategy

- Usage is folded incrementally from ACP/plugin events.
- Rolling windows use bounded buckets, not transcript rescans.
- Policy evaluation runs on relevant usage/capacity changes, not every frame.
- Price data is bundled/versioned; optional refresh is background and cached.

### Acceptance criteria

- Unknown usage cannot trip a monetary hard cap.
- Restart reconstructs bounded-window state without folding an unlimited log.
- A rate-limit event affects only matching capacity pools.
- Policy decisions explain the facts and rule that produced them.

## 7. Campaign and task dependencies

### User outcome

AoE can represent related work and what is blocked without embedding an
autonomous project-management policy in core.

### Core model

- `Campaign`: ID, label, scope, timestamps, optional external reference.
- `TaskNode`: stable ID, title, state, optional session/checkpoint/trial links.
- `DependencyEdge`: predecessor, successor, edge kind.
- States: `planned`, `ready`, `active`, `waiting`, `review`, `done`, `failed`,
  `cancelled`.
- Edge kinds: `blocks`, `informs`, and `conflicts_with`.

The graph module validates cycles in blocking edges, derives the ready frontier,
and emits attention facts when the frontier changes. It does not choose agents,
prompts, or permission modes.

### Plugin policy

Schedulers, Linear/GitHub/Jira import, issue decomposition, auto-dispatch,
completion callbacks, and merge policy belong in plugins using stable host
primitives. Core supplies idempotent creation, readiness, filtering, schemas,
and completion events.

### Interaction

- Compact campaign/task metadata appears on session rows only when present.
- A keyboard-opened list is the primary TUI view; a graph visualization is an
  optional web enhancement, not required to operate the feature.
- `j`/`k`, arrows, `Enter`, `/`, `Tab`, and `Esc` follow the same picker contract
  as the attention and conflict panels.

### Performance strategy

- Maintain predecessor counts and ready-frontier membership incrementally.
- Do not traverse the entire graph on render.
- Blocking-cycle validation runs on graph mutation, bounded to the affected
  reachable subgraph.

### Acceptance criteria

- Duplicate dispatcher requests are idempotent.
- Blocking cycles are rejected with the concrete cycle path.
- Completing one node updates only its successors and one fleet projection.
- Removing an external plugin leaves readable core campaign data.

## Cross-feature behavior

The capabilities reinforce one another through facts rather than direct module
calls:

- Conflict facts enter the attention inbox and readiness projection.
- A checkpoint is the immutable starting state for a Trial.
- Trial verification produces evidence facts and usage samples.
- Governor decisions constrain Trial launches and campaign dispatchers.
- Campaign dependencies can use Trial winners or readiness policy as explicit
  completion conditions supplied by a plugin.

No feature imports another feature's implementation. They meet at typed events
and immutable projections owned by `fleet`.

## Rollout sequence

### Phase 0: safeguards

- Add benchmark fixtures and fleet performance reporting.
- Add keymap collision/ambiguity tests for every new action.
- Establish disabled-feature zero-work tests.

### Phase 1: substrate and conflict radar

- Add `fleet` events, immutable snapshots, projections, and scheduler.
- Connect lifecycle/file-watch/Git adapters.
- Ship read-only conflict radar in CLI, HTTP, TUI, and web.

### Phase 2: attention inbox

- Normalize existing waiting/error/approval/unread signals.
- Add conflict and plugin items.
- Ship keyboard-first queue and jump actions.

### Phase 3: checkpoints

- Add manifests, content-addressed artifacts, inspect/export.
- Add previewed fork/restore operations per agent capability.

### Phase 4: Trials and evidence

- Build Trials over sessions and checkpoints.
- Add verification recipes and local evidence.
- Connect the GitHub plugin evidence adapter.

### Phase 5: governor

- Add usage-quality model, rolling windows, and advisory policies.
- Gate Trial concurrency and optional safe-boundary pauses.

### Phase 6: campaigns

- Add graph metadata, ready frontier, CLI/HTTP/TUI/web views.
- Publish plugin host primitives and a reference scheduler plugin.

Each phase is releasable and reversible. A later phase cannot be used to justify
shipping a slower or incomplete earlier phase.

## Verification matrix

Every phase must cover:

- Rust unit tests at the `fleet` interface;
- persistence migration and downgrade/read-old-data behavior;
- CLI JSON schema snapshots;
- server contract tests and CityHall/read-only denial rules;
- TUI e2e tests for keyboard-only happy path, cancellation, focus isolation,
  narrow terminals, and legacy key encoding;
- web reducer tests, keyboard tests, mocked Playwright, and live Playwright for
  lifecycle flows;
- feature-off tests;
- 1/20/100/500-session benchmarks;
- cancellation and stale-generation races;
- Linux and macOS behavior, with tmux unavailable auto-skip where established.

## Definition of done

The program is complete when all seven capabilities are available through the
shared module, each surface uses the same facts, every mutation has preview and
ownership safeguards, documentation and help are current, and the performance
budgets pass on the project reference runners.

"Implemented" does not mean a panel exists. It means the feature remains fast
with a large fleet, works without a mouse, handles partial/stale data honestly,
survives restart, and is covered at the interface and user-story levels.

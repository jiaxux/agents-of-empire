# Fleet Operations Claude Code Handoff

Fleet Operations is implemented across the Rust TUI/domain layer, CLI, HTTP,
and web dashboard. Continue from this tree. Preserve the existing worktree and
do not rebuild the feature set from scratch.

## First actions

1. Read `AGENTS.md`, `DESIGN.md`, and
   `docs/development/fleet-operations-program.md` completely.
2. Run `git status --short`. This worktree contains a large mixed set of local
   changes and untracked files. Do not reset, clean, or overwrite them.
3. Read `docs/guides/fleet-operations.md` for the shipped behavior and exact
   keyboard map.
4. Inspect the modules listed below before editing their callers.
5. Run the focused verification commands before making structural changes.

## Product state

All seven Fleet capabilities have a working local implementation:

| Capability | Current behavior |
| --- | --- |
| Attention | Ranked session, verification, capacity, conflict, and campaign facts. `a` durably acknowledges one occurrence by opaque ID. |
| Conflicts | Generation-stamped background Git scans, repository-scoped path overlap, `r` refresh, `s` coordination through the existing send-message path. |
| Readiness | Explicit diff, conflict, and Trial verification evidence. Missing external test/format/lint/review producers stay visible as missing. |
| Capacity | Constant-time worker admission and operation reservations based on `acp.max_concurrent_workers`; all observed failure/terminal paths release leases. |
| Checkpoints | Durable capture, inspect, verified export/import domain, reference-counted delete, guarded in-place restore with recovery ref, historical fork, and Trial seed. |
| Trials | Two through eight isolated contenders, mandatory bounded verification recipe, deterministic ranking, retry history, cancellation, retained-winner marker, and restart recovery. |
| Campaigns | Durable tasks, blocking dependencies, cycle rejection, incremental ready frontier, linked ready-task Attention, and keyboard state transitions. |

The shared schema is exposed by `aoe fleet`, `aoe fleet --json`, and
`GET /api/fleet`. The web dashboard lazy loads a read-only Fleet modal and does
not request the endpoint before the modal opens. CityHall hides the entry point
and rejects the endpoint.

## Module map

### UI-independent domain

- `src/fleet/`: attention, conflict, readiness, budget, checkpoint timeline,
  Trial ranking, campaign graph, and the shared serializable report.
- `src/checkpoints.rs`: versioned manifests, redaction, hashed artifacts,
  export/import, reference-counted delete, fork and restore preflight, recovery
  refs, and atomic storage.
- `src/trials.rs`: versioned Trial manifests, monotonic contender state,
  verification recipes and process-group timeouts, deterministic ranking,
  retries, cancellation, retention, and corrupt-manifest isolation.
- `src/campaigns.rs`: versioned campaign/task records, idempotent task producer
  requests, dependency validation, cycle detection, frontier recomputation, and
  atomic storage.
- `src/fleet_acknowledgements.rs`: bounded durable Attention IDs. It stores no
  paths, summaries, prompts, or output.

### TUI adapters

- `src/tui/fleet_state.rs`: incremental session fold and immutable
  `FleetSnapshot`. This is the central truth used by all Fleet panels.
- `src/tui/dialogs/fleet.rs`: keyboard ownership, panel row cache, inline
  editors, and action eligibility.
- `src/tui/fleet_poller.rs`: generation-stamped Git work.
- `src/tui/checkpoint_poller.rs`, `trial_poller.rs`, `campaign_poller.rs`, and
  `attention_ack_poller.rs`: lazy persistence and orchestration workers.
- `src/tui/home/mod.rs` and `src/tui/home/input.rs`: canonical operation paths,
  confirmations, capacity handoff, recovery, and dialog routing.

### Other consumers

- `src/fleet/report.rs`: schema-versioned shared projection.
- `src/cli/fleet.rs`: text and JSON CLI output.
- `src/server/api/fleet.rs`: read-only HTTP handler on `spawn_blocking`.
- `web/src/lib/fleet.ts`: report schema and fetch adapter.
- `web/src/components/FleetOperations.tsx`: lazy read-only web modal.
- `web/tests/live/fleet-operations.spec.ts`: real-server browser contract.

## Invariants to preserve

- No Git, filesystem, tmux, network, durable-store, or process work in render
  or ordinary key handling.
- No Fleet worker exists until its capability is opened or requested.
- Editors and filters own printable input. Global shortcuts must stop at a
  `data-local-keyboard-scope` surface in the web app.
- Repeated identical session folds publish no new snapshot.
- Repository results are generation-stamped. Obsolete results never overwrite
  newer state.
- Status transition from active work to a stable state invalidates one scan.
  Stable ticks do not keep scanning.
- Capacity is reserved before unattended session creation and transferred or
  released in the same lifecycle path.
- Missing token, CI, review, or adapter facts stay unknown or missing. Never
  coerce them to zero or passed.
- Checkpoint restore and delete remain confirmed. Restore revalidates a clean
  one-repository target and preserves a recovery ref before mutation.
- Trial cancellation preserves sessions and worktrees. Winner retention marks
  provenance and does not silently destroy losers.
- Mouse entry points and keyboard entry points route to the same operation.

## Intentional boundaries

These are explicit product limits, not unfinished wiring:

- Checkpoint historical forks and restores support one repository. Partial,
  redacted, corrupt, and multi-repository checkpoints are refused.
- Checkpoint children and Trial contenders start fresh terminal conversations;
  code state is exact, conversation history is not claimed to be forked.
- Campaign transitions are operator-controlled and do not autonomously start
  agents or send prompts.
- The web Fleet modal is read-only. Mutating operations remain in the TUI.
- Token/cost usage is unknown unless an agent adapter reports it.
- Tests, format, lint, and review remain missing readiness evidence until a
  trustworthy producer is connected.

## Performance evidence

`fleet_state_performance_report` is an ignored release-mode distribution test.
On the 2026-08-27 development machine, the latest p50/p95 values were:

| Sessions | Unchanged sync + snapshot | Full rebuild | Snapshot read |
| --- | --- | --- | --- |
| 1 | 0.542 / 0.584 us | 1.625 / 1.750 us | 0.041 / 0.042 us |
| 20 | 6.209 / 7.833 us | 14.541 / 21.250 us | 0.041 / 0.042 us |
| 100 | 20.708 / 32.625 us | 49.833 / 94.625 us | at most 0.042 us |
| 500 | 82.542 / 86.667 us | 234.958 / 244.667 us | at most 0.042 us |

Run it with:

```bash
cargo test --release --lib fleet_state_performance_report -- --ignored --nocapture
```

Normal CI skips elapsed-time assertions because hardware and scheduler noise are
not deterministic. Structural tests enforce dormancy, coalescing, immutable
snapshot reads, and stale-result rejection.

## Focused verification

```bash
cargo fmt --all --check
cargo check --features serve
cargo clippy --lib --features serve -- -D warnings
cargo test --lib --features serve fleet
cargo test --lib checkpoints::tests
cargo test --lib trials::tests
cargo test --lib campaigns::tests
```

For web changes:

```bash
cd web
npm run format:check
npm run lint
npx tsc -b
node tests/validate-coverage-matrix.mjs
npx vitest run src/components/__tests__/TopBar.test.tsx src/hooks/useKeyboardShortcuts.test.ts
AOE_E2E_BINARY=../target/debug/aoe npx playwright test tests/live/fleet-operations.spec.ts --config=playwright.live.config.ts
```

Final local verification on 2026-08-27 produced these results:

- format, diff whitespace, serve build/check, production-library Clippy with
  warnings denied, and generated CLI reference: pass;
- focused Fleet/checkpoint/Trial/campaign batch: 56 passed;
- release performance report: passed at all four fleet sizes;
- web format, lint, TypeScript, and coverage matrix: pass;
- focused web keyboard/top-bar tests: 13 passed;
- live Chromium test against `target/debug/aoe`: 1 passed;
- broad Rust run with the two baseline tests below skipped: 6,472 library tests
  plus every main, integration, and doc-test target passed.

Re-run the relevant gates after any edit and update this section with exact
results.

## Broad release checks

Run `cargo xtask gen-docs` after CLI definition changes. The unfiltered full
suite currently has one deterministic macOS path-alias failure in
`git::worktree::tests::worktree_move_observation_canonicalizes_symlinked_parents`
because `/private/var/...` and `/var/...` refer to the same location. The
process-group SIGTERM test can also miss its marker under full parallel load;
it passed immediately when rerun alone. The broad passing run skipped these
two tests only.

All-target Clippy is blocked by 12 pre-existing test-target warnings in ACP,
macOS process code, Home test-module placement, groups, and smart rename.
Production-library Clippy with the serve feature is clean. Do not weaken lint
settings or rewrite unrelated work merely to hide these baselines. Report exact
current output.

## Git safety

Before publishing, review the worktree, stage only intended paths, and commit
on a feature branch. Never replace an existing remote or stage the whole dirty
tree without reviewing it.

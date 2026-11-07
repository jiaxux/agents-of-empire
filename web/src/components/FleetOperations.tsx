import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { fetchFleet, type FleetReport } from "../lib/fleet";

const TABS = ["attention", "conflicts", "readiness", "campaign", "budget", "trials", "checkpoints"] as const;
type FleetTab = (typeof TABS)[number];

interface Props {
  onClose: () => void;
  onSelectSession: (sessionId: string) => void;
}

interface FleetRow {
  id: string;
  status: string;
  primary: string;
  secondary: string;
  sessionId?: string;
}

export function FleetOperations({ onClose, onSelectSession }: Props) {
  const [report, setReport] = useState<FleetReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<FleetTab>("attention");
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState(0);
  const dialogRef = useRef<HTMLDivElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const controller = new AbortController();
    void fetchFleet(controller.signal)
      .then(setReport)
      .catch((reason: unknown) => {
        if ((reason as { name?: string }).name !== "AbortError") {
          setError(reason instanceof Error ? reason.message : "Fleet Operations failed");
        }
      });
    dialogRef.current?.focus();
    return () => controller.abort();
  }, []);

  const rows = useMemo(() => rowsFor(report, tab), [report, tab]);
  const visibleRows = useMemo(() => {
    const needle = filter.trim().toLocaleLowerCase();
    return needle
      ? rows.filter((row) => `${row.status} ${row.primary} ${row.secondary}`.toLocaleLowerCase().includes(needle))
      : rows;
  }, [filter, rows]);
  const selectedIndex = Math.min(selected, Math.max(0, visibleRows.length - 1));

  const activate = (row: FleetRow | undefined) => {
    if (!row?.sessionId) return;
    onClose();
    onSelectSession(row.sessionId);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    // Fleet owns keys while its modal is open. In particular, do not let the
    // same Escape reach App's global shortcut after it clears this filter.
    // Leaving the default action intact for unhandled keys preserves normal
    // Tab focus navigation.
    event.stopPropagation();
    const editing = event.target instanceof HTMLInputElement;
    if (event.key === "Escape") {
      if (editing || filter) {
        setFilter("");
        dialogRef.current?.focus();
      } else {
        onClose();
      }
      event.preventDefault();
      return;
    }
    if (editing) return;
    if (event.key === "/") {
      filterRef.current?.focus();
      event.preventDefault();
    } else if (event.key === "ArrowLeft" || event.key === "h") {
      setTab((current) => cycleTab(current, -1));
      setSelected(0);
      event.preventDefault();
    } else if (event.key === "ArrowRight" || event.key === "l") {
      setTab((current) => cycleTab(current, 1));
      setSelected(0);
      event.preventDefault();
    } else if (event.key === "ArrowUp" || event.key === "k") {
      setSelected((current) => Math.max(0, current - 1));
      event.preventDefault();
    } else if (event.key === "ArrowDown" || event.key === "j") {
      setSelected((current) => Math.min(Math.max(0, visibleRows.length - 1), current + 1));
      event.preventDefault();
    } else if (event.key === "Enter") {
      activate(visibleRows[selectedIndex]);
      event.preventDefault();
    }
  };

  return (
    <div className="fixed inset-0 z-[90] flex items-center justify-center bg-black/65 p-3 md:p-8" onMouseDown={onClose}>
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="fleet-title"
        tabIndex={-1}
        onKeyDown={onKeyDown}
        data-local-keyboard-scope
        onMouseDown={(event) => event.stopPropagation()}
        className="flex h-[min(760px,92vh)] w-full max-w-5xl flex-col overflow-hidden rounded-xl border border-surface-700/60 bg-surface-900 shadow-2xl outline-none"
        data-testid="fleet-operations"
      >
        <header className="flex items-center gap-3 border-b border-surface-700/60 px-4 py-3">
          <div className="min-w-0 flex-1">
            <h2 id="fleet-title" className="font-mono text-base font-semibold text-text-primary">
              Fleet Operations
            </h2>
            <p className="text-xs text-text-muted">
              {report
                ? `${report.sessions} active session${report.sessions === 1 ? "" : "s"}`
                : "Loading cached facts…"}
            </p>
          </div>
          <input
            ref={filterRef}
            value={filter}
            onChange={(event) => {
              setFilter(event.target.value);
              setSelected(0);
            }}
            placeholder="Filter /"
            aria-label="Filter Fleet Operations"
            className="w-32 rounded-md border border-surface-700/60 bg-surface-950 px-2 py-1.5 font-mono text-xs text-text-primary outline-none focus:border-brand-600 md:w-52"
          />
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-2 py-1 text-sm text-text-muted hover:bg-surface-800 hover:text-text-primary"
            aria-label="Close Fleet Operations"
          >
            Esc
          </button>
        </header>

        <div
          role="tablist"
          aria-label="Fleet panels"
          className="flex overflow-x-auto border-b border-surface-700/60 px-2"
        >
          {TABS.map((candidate) => (
            <button
              key={candidate}
              type="button"
              role="tab"
              aria-selected={tab === candidate}
              onClick={() => {
                setTab(candidate);
                setSelected(0);
                dialogRef.current?.focus();
              }}
              className={`shrink-0 border-b-2 px-3 py-2 font-mono text-xs capitalize ${
                tab === candidate
                  ? "border-brand-500 text-text-primary"
                  : "border-transparent text-text-muted hover:text-text-secondary"
              }`}
            >
              {candidate}
              <span className="ml-1 text-text-dim">{countFor(report, candidate)}</span>
            </button>
          ))}
        </div>

        <main className="min-h-0 flex-1 overflow-y-auto p-2" role="tabpanel">
          {error ? (
            <EmptyState text={error} />
          ) : !report ? (
            <EmptyState text="Building the projection in the background…" />
          ) : visibleRows.length === 0 ? (
            <EmptyState text={filter ? "No rows match this filter." : emptyText(tab)} />
          ) : (
            <div className="space-y-1">
              {visibleRows.map((row, index) => (
                <button
                  key={row.id}
                  type="button"
                  onClick={() => activate(row)}
                  disabled={!row.sessionId}
                  className={`flex w-full items-start gap-3 rounded-md px-3 py-2 text-left ${
                    index === selectedIndex ? "bg-surface-800 ring-1 ring-brand-600/50" : "hover:bg-surface-850"
                  } ${row.sessionId ? "cursor-pointer" : "cursor-default"}`}
                >
                  <span className="w-24 shrink-0 font-mono text-xs text-brand-400">{row.status}</span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-sm text-text-primary">{row.primary}</span>
                    <span className="block truncate text-xs text-text-muted">{row.secondary}</span>
                  </span>
                </button>
              ))}
            </div>
          )}
        </main>

        <footer className="border-t border-surface-700/60 px-4 py-2 font-mono text-[11px] text-text-dim">
          h/l panels · j/k rows · Enter open session · / filter · Esc close
        </footer>
      </div>
    </div>
  );
}

function EmptyState({ text }: { text: string }) {
  return <p className="p-8 text-center text-sm text-text-muted">{text}</p>;
}

function cycleTab(tab: FleetTab, delta: number): FleetTab {
  const index = TABS.indexOf(tab);
  return TABS[(index + delta + TABS.length) % TABS.length]!;
}

function countFor(report: FleetReport | null, tab: FleetTab): number {
  if (!report) return 0;
  switch (tab) {
    case "attention":
      return report.attention.length;
    case "conflicts":
      return report.conflicts.length;
    case "readiness":
      return report.readiness.length;
    case "campaign":
      return report.campaign_tasks.length;
    case "budget":
      return report.sessions;
    case "trials":
      return report.trials.length;
    case "checkpoints":
      return report.checkpoints.length;
  }
}

function rowsFor(report: FleetReport | null, tab: FleetTab): FleetRow[] {
  if (!report) return [];
  switch (tab) {
    case "attention":
      return report.attention.map((item) => ({
        id: item.item_id,
        status: item.reason,
        primary: item.title,
        secondary: item.summary,
        sessionId: item.session_id,
      }));
    case "conflicts":
      return report.conflicts.map((item) => ({
        id: `${item.repository}:${item.path}`,
        status: "overlap",
        primary: `${item.repository}/${item.path}`,
        secondary: item.sessions.join(", "),
        sessionId: item.session_ids[0],
      }));
    case "readiness":
      return report.readiness.map((item) => ({
        id: item.session_id,
        status: item.verdict,
        primary: item.title,
        secondary: item.evidence.map((fact) => `${fact.kind}=${fact.state}`).join(" · "),
        sessionId: item.session_id,
      }));
    case "campaign":
      return report.campaign_tasks.map((item) => ({
        id: item.id,
        status: item.state,
        primary: item.title,
        secondary: `${item.campaign_label}${item.blocked_by_failure ? " · blocked by failed dependency" : ""}`,
        sessionId: item.links.session_id,
      }));
    case "budget":
      return [
        {
          id: "workers",
          status: "workers",
          primary: `${report.budget.active_workers}/${report.budget.max_active_workers ?? "?"} active`,
          secondary: `tokens ${report.budget.reported_tokens ?? "unknown"} · ${report.budget.rejected.length} rejected`,
        },
      ];
    case "trials":
      return report.trials.flatMap((trial) =>
        trial.contenders.map((contender) => {
          const rank = trial.ranking.find((entry) => entry.contender_id === contender.id)?.rank;
          return {
            id: `${trial.id}:${contender.id}`,
            status: contender.retained ? "retained" : contender.state,
            primary: `${contender.tool}${rank ? ` · #${rank}` : ""}`,
            secondary: `${trial.prompt} · verify ${contender.verification?.state ?? "unknown"} · ${
              contender.changed_files ?? "?"
            } files · ${contender.elapsed_ms ?? "?"} ms`,
            sessionId: contender.session_id ?? undefined,
          };
        }),
      );
    case "checkpoints":
      return report.checkpoints.map((item) => ({
        id: item.checkpoint_id,
        status: item.state,
        primary: `${item.title} · ${item.label || "unlabeled"}`,
        secondary: item.detail ?? item.snapshot_ref,
        sessionId: item.session_available ? item.session_id : undefined,
      }));
  }
}

function emptyText(tab: FleetTab): string {
  switch (tab) {
    case "attention":
      return "Nothing needs your attention.";
    case "conflicts":
      return "No overlapping changed paths.";
    case "readiness":
      return "No active sessions to evaluate.";
    case "campaign":
      return "No campaign tasks recorded.";
    case "budget":
      return "No capacity information.";
    case "trials":
      return "No Trials recorded.";
    case "checkpoints":
      return "No checkpoints recorded.";
  }
}

export interface FleetReport {
  schema_version: number;
  generated_at_ms: number;
  sessions: number;
  attention: Array<{
    item_id: string;
    session_id: string;
    title: string;
    reason: string;
    summary: string;
    since_ms: number;
  }>;
  conflicts: Array<{
    repository: string;
    path: string;
    session_ids: string[];
    sessions: string[];
  }>;
  readiness: Array<{
    session_id: string;
    title: string;
    verdict: string;
    blockers: string[];
    evidence: Array<{
      kind: string;
      state: string;
      observed_at_ms: number;
      summary: string;
    }>;
  }>;
  budget: {
    active_workers: number;
    max_active_workers: number | null;
    active_weight: number;
    reported_tokens: number | null;
    rejected: string[];
  };
  campaign: {
    total: number;
    ready: number;
    running: number;
    completed: number;
    failed: number;
    blocked: number;
  } | null;
  campaign_tasks: Array<{
    campaign_id: string;
    campaign_label: string;
    id: string;
    title: string;
    state: string;
    ready: boolean;
    blocked_by_failure: boolean;
    links: { session_id?: string; checkpoint_id?: string; trial_id?: string };
  }>;
  trials: Array<{
    id: string;
    prompt: string;
    state: string;
    completed: number;
    failed: number;
    total: number;
    contenders: Array<{
      id: string;
      tool: string;
      state: string;
      session_id: string | null;
      elapsed_ms: number | null;
      reported_tokens: number | null;
      changed_files: number | null;
      retained: boolean;
      verification: { state: string; summary: string } | null;
    }>;
    ranking: Array<{ rank: number; contender_id: string; session_id: string }>;
  }>;
  checkpoints: Array<{
    checkpoint_id: string;
    session_id: string;
    session_available: boolean;
    title: string;
    label: string;
    snapshot_ref: string;
    state: string;
    detail: string | null;
  }>;
  scan: { state: string; scanned: number; skipped: number; errors: string[] };
}

export async function fetchFleet(signal?: AbortSignal): Promise<FleetReport> {
  const response = await fetch("/api/fleet", { signal });
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { message?: string } | null;
    throw new Error(body?.message ?? `Fleet Operations failed (${response.status})`);
  }
  return (await response.json()) as FleetReport;
}

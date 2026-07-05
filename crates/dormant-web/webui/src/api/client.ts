/**
 * Typed fetch wrappers for the dormant daemon HTTP API.
 *
 * Every endpoint returns JSON.  Errors surface as rejected Promises
 * with a descriptive message; callers handle them at the view level
 * (toast, disabled state, retry button).
 *
 * The Vite dev proxy routes /api → the daemon; in production the
 * same origin serves both the SPA and /api (axum + rust-embed).
 */
import type {
  StateSnapshot,
  ConfigResponse,
  DoctorReport,
} from "./types";

// ── Shared helpers ──────────────────────────────────────────────────────

const BASE = "/api";

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(BASE + url, {
    headers: { "Accept": "application/json" },
    ...init,
  });

  if (!res.ok) {
    const body = await res.text().catch(() => "(no body)");
    throw new Error(`API ${res.status} on ${url}: ${body}`);
  }

  return res.json() as Promise<T>;
}

// ── Read endpoints ──────────────────────────────────────────────────────

/** GET /api/status — full engine snapshot (spec §4.1). */
export function getState(): Promise<StateSnapshot> {
  return request<StateSnapshot>("/status");
}

/** GET /api/config — parsed config inventory + raw TOML (spec §4.1). */
export function getConfig(): Promise<ConfigResponse> {
  return request<ConfigResponse>("/config");
}

// ── Write endpoints ─────────────────────────────────────────────────────

/** POST /api/displays/:display/blank — force-blank a display. */
export function postBlank(display: string): Promise<void> {
  return request<void>(`/displays/${encodeURIComponent(display)}/blank`, {
    method: "POST",
  });
}

/** POST /api/displays/:display/wake — force-wake a display. */
export function postWake(display: string): Promise<void> {
  return request<void>(`/displays/${encodeURIComponent(display)}/wake`, {
    method: "POST",
  });
}

/** POST /api/pause — pause rules (spec: `pause_all` + optional `rule` / `duration_s`). */
export function postPause(opts?: {
  rule?: string;
  duration_s?: number | null;
}): Promise<void> {
  return request<void>("/pause", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      ...(opts?.rule != null ? { rule: opts.rule } : { pause_all: true }),
      ...(opts?.duration_s !== undefined ? { duration_s: opts.duration_s } : {}),
    }),
  });
}

/** POST /api/resume — resume rules (spec: `resume_all` + optional `rule`). */
export function postResume(opts?: { rule?: string }): Promise<void> {
  return request<void>("/resume", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(
      opts?.rule != null ? { rule: opts.rule } : { resume_all: true },
    ),
  });
}

/** POST /api/reload — hot-reload the daemon config. */
export function postReload(): Promise<void> {
  return request<void>("/reload", {
    method: "POST",
  });
}

/** POST /api/doctor — run the diagnosis probes. */
export function runDoctor(): Promise<DoctorReport> {
  return request<DoctorReport>("/doctor", {
    method: "POST",
  });
}

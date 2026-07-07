/**
 * Typed fetch wrappers for the dormant daemon HTTP API.
 *
 * Route verification (server.rs + command.rs):
 *   GET  /api/state   → StateSnapshot
 *   GET  /api/config  → ConfigResponse
 *   POST /api/blank   → JSON { display: "<id>" }
 *   POST /api/wake    → JSON { display: "<id>" }
 *   POST /api/pause   → JSON { rule?: string, duration_s?: number }
 *   POST /api/resume  → JSON { rule?: string }
 *   POST /api/reload  → no body extractor; Content-Type header required by guard
 *   POST /api/doctor  → no body extractor; Content-Type header required by guard
 *
 * All POSTs MUST send Content-Type: application/json — the security
 * guard (security.rs:60-71) rejects POSTs without it (415).
 */
import type {
  StateSnapshot,
  ConfigResponse,
  DoctorReport,
  ApplyRequest,
  ApplyResponse,
} from "./types";

export type { ApplyErrorBody, ConfigApplyErrorDetail, ApplyConflictBody } from "./types";

const BASE = "/api";
const JSON_CT = { "Content-Type": "application/json" };

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(BASE + url, init);

  if (!res.ok) {
    const body = await res.text().catch(() => "(no body)");
    throw new Error(`API ${res.status} on ${url}: ${body}`);
  }

  return res.json() as Promise<T>;
}

/** Typed error that carries HTTP status and parsed body for structured error handling. */
export class ApiError extends Error {
  status: number;
  body: unknown;

  constructor(status: number, body: unknown) {
    super(`API ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }
}

export function getState(): Promise<StateSnapshot> {
  return request<StateSnapshot>("/state");
}

export function getConfig(): Promise<ConfigResponse> {
  return request<ConfigResponse>("/config");
}

/** POST /api/blank — force-blank a display by id. */
export function postBlank(display: string): Promise<void> {
  return request<void>("/blank", {
    method: "POST",
    headers: JSON_CT,
    body: JSON.stringify({ display }),
  });
}

/** POST /api/wake — force-wake a display by id. */
export function postWake(display: string): Promise<void> {
  return request<void>("/wake", {
    method: "POST",
    headers: JSON_CT,
    body: JSON.stringify({ display }),
  });
}

/** POST /api/pause — pause blanking.  Omit `rule` to pause all rules. */
export function postPause(opts?: {
  rule?: string;
  duration_s?: number | null;
}): Promise<void> {
  return request<void>("/pause", {
    method: "POST",
    headers: JSON_CT,
    body: JSON.stringify({
      ...(opts?.rule != null ? { rule: opts.rule } : {}),
      ...(opts?.duration_s !== undefined ? { duration_s: opts.duration_s } : {}),
    }),
  });
}

/** POST /api/resume — resume blanking.  Omit `rule` to resume all rules. */
export function postResume(opts?: { rule?: string }): Promise<void> {
  return request<void>("/resume", {
    method: "POST",
    headers: JSON_CT,
    body: JSON.stringify(opts?.rule != null ? { rule: opts.rule } : {}),
  });
}

/** POST /api/reload — hot-reload the daemon config. */
export function postReload(): Promise<void> {
  return request<void>("/reload", { method: "POST", headers: JSON_CT, body: "{}" });
}

/** POST /api/doctor — run the diagnosis probes. */
export function runDoctor(): Promise<DoctorReport> {
  return request<DoctorReport>("/doctor", { method: "POST", headers: JSON_CT, body: "{}" });
}

/** POST /api/config/apply — apply a set of patches to the live config. */
export async function postConfigApply(req: ApplyRequest): Promise<ApplyResponse> {
  const res = await fetch(BASE + "/config/apply", {
    method: "POST",
    headers: JSON_CT,
    body: JSON.stringify(req),
  });

  const body = await res.json().catch(() => null);

  if (!res.ok) {
    throw new ApiError(res.status, body);
  }

  return body as ApplyResponse;
}

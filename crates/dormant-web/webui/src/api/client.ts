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
 *   POST /api/emergency-wake → no body extractor; returns EmergencyWakeReport
 *   POST /api/doctor/exercise/:display → no body extractor; returns ExerciseReport
 *   GET  /api/operations → OperationsStatus
 *   GET  /api/daemon → DaemonIdentity
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
  WearListResponse,
  WearDetail,
  PairAccepted,
  PairStatus,
  EmergencyWakeReport,
  ExerciseReport,
  OperationsStatus,
  DaemonIdentity,
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

/** GET /api/wear — every tracked display's panel-exposure summary. */
export function getWear(): Promise<WearListResponse> {
  return request<WearListResponse>("/wear");
}

/** GET /api/wear/:display — one display's summary plus its wear grid. */
export function getWearDetail(display: string): Promise<WearDetail> {
  return request<WearDetail>(`/wear/${encodeURIComponent(display)}`);
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

/**
 * POST /api/pair/samsung — start a Samsung Tizen pairing attempt
 * (spec §8). Returns 202 + `{ pair_id }` immediately; the actual TV I/O
 * runs in a spawned server-side task, polled via `getPairStatus`.
 *
 * Errors surface as `ApiError` (409 `pairing_in_progress`, 403
 * `feature_disabled`/`web_reject_origin`, 413 body-too-large) —
 * identical error-handling contract to `postConfigApply`.
 */
export async function postPairSamsung(host: string): Promise<PairAccepted> {
  const res = await fetch(BASE + "/pair/samsung", {
    method: "POST",
    headers: JSON_CT,
    body: JSON.stringify({ host }),
  });

  const body = await res.json().catch(() => null);

  if (!res.ok) {
    throw new ApiError(res.status, body);
  }

  return body as PairAccepted;
}

/**
 * GET /api/pair/samsung/{id} — poll a pairing attempt's status
 * (spec §8.2). The response NEVER carries a token field, by server-side
 * construction — this is a read route (weaker-origin-OK), safe to poll
 * on an interval.
 */
export function getPairStatus(pairId: string): Promise<PairStatus> {
  return request<PairStatus>(`/pair/samsung/${encodeURIComponent(pairId)}`);
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

/**
 * Shared status-preserving POST helper for the bodyless report endpoints
 * (currently just `postEmergencyWake`). Keeping the HTTP status in
 * `ApiError` — rather than the older text-only `request()` error path —
 * is required so a caller can distinguish 409 (already in progress) from
 * 504 (report timed out; the operation may still be running).
 */
async function postJsonReport<T>(url: string): Promise<T> {
  const res = await fetch(BASE + url, {
    method: "POST",
    headers: JSON_CT,
    body: "{}",
  });
  const body = await res.json().catch(() => null);
  if (!res.ok) throw new ApiError(res.status, body);
  return body as T;
}

/** POST /api/emergency-wake — pause every rule and wake every configured display. */
export function postEmergencyWake(): Promise<EmergencyWakeReport> {
  return postJsonReport<EmergencyWakeReport>("/emergency-wake");
}

/** POST /api/doctor/exercise/:display — run the real blank/read/wake/read/restore sequence. */
export function postExercise(display: string): Promise<ExerciseReport> {
  return postJsonReport<ExerciseReport>(`/doctor/exercise/${encodeURIComponent(display)}`);
}

/** GET /api/operations — authoritative WebState single-flight guard status. */
export function getOperations(): Promise<OperationsStatus> {
  return request<OperationsStatus>("/operations");
}

/** GET /api/daemon — daemon process identity (pid, uptime, version, socket). */
export function getDaemon(): Promise<DaemonIdentity> {
  return request<DaemonIdentity>("/daemon");
}

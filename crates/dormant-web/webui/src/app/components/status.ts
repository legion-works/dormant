/**
 * State → human-readable label mapping.
 *
 * Used by StatusChip and anywhere that needs to display a
 * state string (Dashboard sensor/zone rows, Events badges).
 */
import type { StatusKind } from "./StatusChip";

const STATE_LABELS: Record<string, string> = {
  present: "present",
  absent: "absent",
  unavailable: "unavailable",
  active: "active",
  grace: "grace",
  blanking: "blanking…",
  blanked: "blanked",
  waking: "waking",
  paused: "paused",
  inhibited: "inhibited",
  ok: "ok",
  fail: "fail",
  wake_retry: "retry",
};

export function statusLabel(kind: StatusKind): string {
  return STATE_LABELS[kind] ?? kind;
}

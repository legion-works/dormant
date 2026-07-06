/**
 * State → color mapping pill used across all views.
 *
 * Maps a "state kind" (present/absent/active/blanked/grace/…
 * plus special paused/inhibited/fail) to a DS semantic token
 * class.  Consumers pass the state string; the chip renders
 * a dot + label in the matching color.
 *
 * Color mapping (authoritative — spec §7 + DS tokens):
 *   present | active | waking | ok           → --success (green)
 *   absent | blanked                         → --blue-400 (blue)
 *   grace | blanking | unavailable           → --warning (yellow)
 *   paused                                   → --accent-warm (amber)
 *   inhibited                                → --purple-400 (purple)
 *   fail | wake_retry                        → --danger (red)
 */
import { statusLabel } from "./status";
import "./StatusChip.css";

export type StatusKind =
  | "present"
  | "absent"
  | "unavailable"
  | "active"
  | "grace"
  | "blanking"
  | "blanked"
  | "waking"
  | "paused"
  | "inhibited"
  | "ok"
  | "fail"
  | "wake_retry"
  | "skip"
  | "not_supported"
  | string;

interface StatusChipProps {
  /** The state/phase value to map. */
  kind: StatusKind;
  /** Human-readable label (defaults to kind). */
  label?: string;
  /** Show a dot inside the chip (default true). */
  dot?: boolean;
  /** Extra class names appended to the root element. */
  className?: string;
}

/** Per-kind DS token class.  The root element gets `status-chip--<class>`. */
const STATUS_CLASS_MAP: Record<string, string> = {
  // Green — present / awake / active / ok
  present: "success",
  active: "success",
  waking: "success",
  ok: "success",
  // Blue — absent / blanked
  absent: "blue",
  blanked: "blue",
  // Yellow — grace / blanking / unavailable
  grace: "warning",
  blanking: "warning",
  unavailable: "warning",
  // Amber — paused
  paused: "amber",
  // Purple — inhibited
  inhibited: "purple",
  // Red — fail / wake_retry
  fail: "danger",
  wake_retry: "danger",
  // Neutral — skip / not_supported (doctor checks)
  skip: "muted",
  not_supported: "muted",
};

function statusClass(kind: StatusKind): string {
  return STATUS_CLASS_MAP[kind] ?? "muted";
}

export default function StatusChip({ kind, label, dot = true, className = "" }: StatusChipProps) {
  const cls = statusClass(kind);
  const displayLabel = label ?? statusLabel(kind);

  return (
    <span
      className={`status-chip status-chip--${cls}${className ? " " + className : ""}`}
    >
      {dot && <span className="status-chip__dot" />}
      <span>{displayLabel}</span>
    </span>
  );
}

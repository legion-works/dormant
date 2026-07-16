/**
 * Event badge + message formatters — single source of truth for both the
 * Dashboard Recent Activity section and the full Events view.  No inline
 * style varies independently; any change here applies to both surfaces.
 */
import type { DaemonEvent } from "../../api/types";

export interface EventBadge {
  color: string;
  bg: string;
  label: string;
}

export function badgeForEvent(ev: DaemonEvent): EventBadge {
  switch (ev.event) {
    case "sensor_changed":
      return { color: "var(--blue-400)", bg: "color-mix(in oklab, var(--blue-400) 14%, transparent)", label: "sensor_changed" };
    case "zone_changed":
      return { color: "var(--success)", bg: "color-mix(in oklab, var(--success) 14%, transparent)", label: "zone_changed" };
    case "display_phase":
      return { color: "var(--text-muted)", bg: "color-mix(in oklab, var(--text-muted) 14%, transparent)", label: "display_phase" };
    case "wake_retry":
      return { color: "var(--danger)", bg: "color-mix(in oklab, var(--danger) 14%, transparent)", label: "wake_retry" };
    case "config_reloaded":
      return { color: "var(--accent-warm)", bg: "var(--accent-warm-muted)", label: "config_reloaded" };
    case "config_reload_rejected":
      return { color: "var(--danger)", bg: "color-mix(in oklab, var(--danger) 14%, transparent)", label: "config_reload_rejected" };
    case "wear_snapshot":
      return { color: "var(--purple-400)", bg: "color-mix(in oklab, var(--purple-400) 14%, transparent)", label: "wear_snapshot" };
    case "compensation_advisory":
      return { color: "var(--warning)", bg: "color-mix(in oklab, var(--warning) 14%, transparent)", label: "compensation_advisory" };
    case "blank_failure":
      return { color: "var(--danger)", bg: "color-mix(in oklab, var(--danger) 14%, transparent)", label: "blank_failure" };
    case "blank_recovered":
      return { color: "var(--success)", bg: "color-mix(in oklab, var(--success) 14%, transparent)", label: "blank_recovered" };
    case "wake_recovered":
      return { color: "var(--success)", bg: "color-mix(in oklab, var(--success) 14%, transparent)", label: "wake_recovered" };
    default:
      return { color: "var(--text-muted)", bg: "var(--bg-sunken)", label: (ev as { event: string }).event };
  }
}

export function messageForEvent(ev: DaemonEvent): string {
  switch (ev.event) {
    case "sensor_changed":
      return `${ev.sensor} \u2192 ${ev.state}`;
    case "zone_changed":
      return `zone '${ev.zone}' \u2192 ${ev.present ? "occupied" : "vacant"} (cause: ${ev.cause})`;
    case "display_phase":
      return `${ev.display}: ${ev.phase} (cause: ${ev.cause})`;
    case "config_reloaded":
      return "config reloaded";
    case "config_reload_rejected":
      return `config reload rejected: ${ev.detail}`;
    case "wake_retry":
      return `${ev.display}: wake retry attempt ${ev.attempt}`;
    case "wear_snapshot":
      return `${ev.display}: ${ev.total_on_hours.toFixed(1)}h total on-time (${ev.sample_count} samples)`;
    case "compensation_advisory": {
      const days = Math.floor(ev.hours_since_long_dwell / 24);
      return `${ev.display}: no long standby window in ${days} days`;
    }
    case "blank_failure":
      return `${ev.display}: blank failed on ${ev.controller} (${ev.detail})`;
    case "blank_recovered":
      return `${ev.display}: blank recovered`;
    case "wake_recovered":
      return `${ev.display}: wake recovered after ${ev.attempts} attempt${ev.attempts === 1 ? "" : "s"}`;
    default:
      return JSON.stringify(ev);
  }
}

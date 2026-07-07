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
      return { color: "var(--blue-400)", bg: "color-mix(in oklab, var(--blue-400) 14%, transparent)", label: "sensor" };
    case "zone_changed":
      return { color: "var(--success)", bg: "color-mix(in oklab, var(--success) 14%, transparent)", label: "zone" };
    case "display_phase":
      return { color: "var(--text-muted)", bg: "color-mix(in oklab, var(--text-muted) 14%, transparent)", label: "display" };
    case "wake_retry":
      return { color: "var(--danger)", bg: "color-mix(in oklab, var(--danger) 14%, transparent)", label: "retry" };
    case "config_reloaded":
      return { color: "var(--accent-warm)", bg: "var(--accent-warm-muted)", label: "config" };
    case "config_reload_rejected":
      return { color: "var(--danger)", bg: "color-mix(in oklab, var(--danger) 14%, transparent)", label: "config" };
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
    default:
      return JSON.stringify(ev);
  }
}

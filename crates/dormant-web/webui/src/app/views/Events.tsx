/**
 * Events view — live DaemonEvent log from the WS stream.
 *
 * Subscribes to useEvents and maintains a rolling log (newest first,
 * capped at MAX_EVENTS).  Each event is rendered per its variant with a
 * type-colored badge and a human-readable message.
 */
import { useEventLog } from "../hooks/useLiveState";
import type { DaemonEvent } from "../../api/types";
import { Card } from "../components";
import "./Events.css";

interface EventBadge {
  color: string;
  bg: string;
  label: string;
}

function badgeForEvent(ev: DaemonEvent): EventBadge {
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
    default:
      // unreachable for typed DaemonEvent variants; the fallback handles future wire additions
      return { color: "var(--text-muted)", bg: "var(--bg-sunken)", label: (ev as { event: string }).event };
  }
}

function messageForEvent(ev: DaemonEvent): string {
  switch (ev.event) {
    case "sensor_changed":
      return `${ev.sensor} \u2192 ${ev.state}`;
    case "zone_changed":
      return `zone '${ev.zone}' \u2192 ${ev.present ? "occupied" : "vacant"} (cause: ${ev.cause})`;
    case "display_phase":
      return `${ev.display}: ${ev.phase} (cause: ${ev.cause})`;
    case "config_reloaded":
      return "config reloaded";
    case "wake_retry":
      return `${ev.display}: wake retry attempt ${ev.attempt}`;
    default:
      return JSON.stringify(ev);
  }
}

export default function Events() {
  // Previously managed local event + lag state; now reads from the
  // shared provider which owns the WS connection and event log.
  const { events, connected, lagged } = useEventLog();

  const showBanner = !connected || lagged;
  const bannerText = lagged
    ? "stream lagged — catching up"
    : "reconnecting to daemon…";

  return (
    <div className="events">
      <div className="events-header">
        <div className="events-header__left">
          <span className={`events-pulse${connected ? " events-pulse--live" : ""}`} />
          <span className="events-header__label">
            {connected ? "live · subscribed to daemon event stream" : "disconnected"}
          </span>
        </div>
        <span className="events-header__count">{events.length} events</span>
      </div>

      {showBanner && (
        <div className={`events-banner${lagged ? " events-banner--lag" : ""}`}>
          {bannerText}
        </div>
      )}

      <Card opaque>
        {events.length === 0 ? (
          <div className="events-empty">
            {connected
              ? "Waiting for events from the daemon…"
              : "Connecting to daemon…"}
          </div>
        ) : (
          events.map((se, i) => {
            const badge = badgeForEvent(se.event);
            const msg = messageForEvent(se.event);
            return (
              <div key={`${se.time}-${i}`} className="events-row">
                <span className="events-row__time">{se.time}</span>
                <span
                  className="events-row__badge"
                  style={{ color: badge.color, backgroundColor: badge.bg }}
                >
                  {badge.label}
                </span>
                <span className="events-row__msg">{msg}</span>
              </div>
            );
          })
        )}
      </Card>
    </div>
  );
}

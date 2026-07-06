/**
 * Events view — live DaemonEvent log from the WS stream.
 *
 * Subscribes to useEvents and maintains a rolling log (newest first,
 * capped at 100).  Each event is rendered per its variant with a
 * type-colored badge and a human-readable message.
 *
 * Data: WS /api/events  |  Visual authority: design README §3 /
 * Dormant Dashboard.dc.html lines 250-268.
 */
import { useState, useCallback, useRef, useEffect } from "react";
import { useEvents } from "../../api/ws";
import type { DaemonEvent } from "../../api/types";
import { Card } from "../components";
import "./Events.css";

/** Render cap — oldest events drop off when exceeded. */
const MAX_EVENTS = 100;

interface StampedEvent {
  /** ISO time string captured at arrival. */
  time: string;
  event: DaemonEvent;
}

function formatTimestamp(): string {
  return new Date().toLocaleTimeString("en-GB", { hour12: false });
}

/**
 * Event tag → DS token color mapping.
 * Colors match the handoff README §3 event-type badge spec:
 *   zone_change   → green  (--success)
 *   sensor_change → blue   (--blue-400)
 *   display_phase → grey   (--text-muted)
 *   wake_retry    → red    (--danger)
 *   config_reload → amber  (--accent-warm)
 *   pause         → amber  (--accent-warm)
 *   resume        → green  (--success)
 */
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
  const [events, setEvents] = useState<StampedEvent[]>([]);
  const [lagged, setLagged] = useState(false);
  const lagTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const onMessage = useCallback((data: unknown) => {
    const ev = data as DaemonEvent;

    // The daemon may send {"event":"stream_lagged",...} — surface the banner
    // for a few seconds.
    if (ev && typeof ev === "object" && "event" in ev && (ev as { event: string }).event === "stream_lagged") {
      setLagged(true);
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
      lagTimerRef.current = setTimeout(() => setLagged(false), 5_000);
      return;
    }

    if (ev && typeof ev === "object" && "event" in ev) {
      setEvents((prev) => {
        const next = [{ time: formatTimestamp(), event: ev }, ...prev];
        return next.length > MAX_EVENTS ? next.slice(0, MAX_EVENTS) : next;
      });
    }
  }, []);

  const onConnect = useCallback(() => {
    setLagged(false);
    if (lagTimerRef.current != null) {
      clearTimeout(lagTimerRef.current);
      lagTimerRef.current = null;
    }
  }, []);

  useEffect(() => {
    return () => {
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
    };
  }, []);

  const { connected } = useEvents({ onMessage, onConnect });

  const showBanner = !connected || lagged;
  const bannerText = lagged
    ? "stream lagged — catching up"
    : "reconnecting to daemon…";

  return (
    <div className="events">
      {/* Header */}
      <div className="events-header">
        <div className="events-header__left">
          <span className={`events-pulse${connected ? " events-pulse--live" : ""}`} />
          <span className="events-header__label">
            {connected ? "live · subscribed to daemon event stream" : "disconnected"}
          </span>
        </div>
        <span className="events-header__count">{events.length} events</span>
      </div>

      {/* Lag / disconnect banner */}
      {showBanner && (
        <div className={`events-banner${lagged ? " events-banner--lag" : ""}`}>
          {bannerText}
        </div>
      )}

      {/* Event log */}
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

/**
 * Events view — live DaemonEvent log from the WS stream.
 *
 * Subscribes to useEvents and maintains a rolling log (newest first,
 * capped at MAX_EVENTS).  Each event is rendered per its variant with a
 * type-colored badge and a human-readable message.
 */
import { useEventLog } from "../hooks/useLiveState";
import { Card } from "../components";
import { badgeForEvent, messageForEvent } from "./eventFormat";
import "./Events.css"

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

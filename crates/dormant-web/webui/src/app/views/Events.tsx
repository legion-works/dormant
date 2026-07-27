/**
 * Events view — live DaemonEvent log from the WS stream.
 *
 * Filter groups: displays, presence, failures, switching, wear, config.
 * Tags are additive chips; state lives in the hash query.  The switching
 * group is hidden until an ownership event has arrived.
 *
 * Until BG-2 lands, the view shows an honest no-history notice.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useEventLog } from "../hooks/useLiveState";
import { Card } from "../components";
import { badgeForEvent, messageForEvent } from "./eventFormat";
import type { DaemonEvent } from "../../api/types";
import "./Events.css";

// ── Filter groups ──────────────────────────────────────────────────────────

interface FilterGroup {
  id: string;
  label: string;
  tags: string[];
  hidden?: boolean;
}

const FILTER_GROUPS: FilterGroup[] = [
  { id: "displays", label: "displays", tags: ["display_phase", "wake_retry", "wake_recovered"] },
  { id: "presence", label: "presence", tags: ["sensor_changed", "zone_changed"] },
  { id: "failures", label: "failures", tags: ["blank_failure", "blank_recovered", "config_reload_rejected"] },
  { id: "switching", label: "switching", tags: ["ownership", "hook_fired"], hidden: true },
  { id: "wear", label: "wear", tags: ["wear_snapshot", "compensation_advisory"] },
  { id: "config", label: "config", tags: ["config_reloaded", "config_reload_rejected"] },
];

// ── Hash query helpers ─────────────────────────────────────────────────────

function hashParams(): URLSearchParams {
  const hash = window.location.hash.replace(/^#\/?[^?]*\??/, "");
  return new URLSearchParams(hash);
}

function readFilterTagsFromHash(): Set<string> {
  const tagParam = hashParams().get("tag");
  if (!tagParam) return new Set();
  return new Set(tagParam.split(",").filter(Boolean));
}

// ── Component ──────────────────────────────────────────────────────────────

export default function Events() {
  const { events, connected, lagged } = useEventLog();
  const [activeTags, setActiveTags] = useState<Set<string>>(readFilterTagsFromHash);

  // Sync hash changes (back/forward navigation)
  useEffect(() => {
    const onHashChange = () => setActiveTags(readFilterTagsFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  // Write filter state to hash
  const toggleTag = useCallback((tag: string) => {
    setActiveTags((prev) => {
      const next = new Set(prev);
      if (next.has(tag)) {
        next.delete(tag);
      } else {
        next.add(tag);
      }
      // Update hash
      const params = new URLSearchParams();
      if (next.size > 0) {
        params.set("tag", [...next].join(","));
      }
      const base = window.location.hash.replace(/\?.*/, "");
      const newHash = next.size > 0 ? `${base}?${params.toString()}` : base;
      // Use replaceState to avoid creating history entries per toggle
      window.history.replaceState(null, "", `#/${newHash}`);
      return next;
    });
  }, []);

  // Clear all filters
  const clearFilters = useCallback(() => {
    setActiveTags(new Set());
    const base = window.location.hash.replace(/\?.*/, "");
    window.history.replaceState(null, "", `#/${base}`);
  }, []);

  // Determine if switching group should be shown (at least one ownership/hook_fired event has arrived).
  const hasSwitchingEvents = useMemo(
    () => events.some((se) => se.event.event === "ownership" || (se.event as { event: string }).event === "hook_fired"),
    [events],
  );

  const visibleGroups = FILTER_GROUPS.map((g) =>
    g.id === "switching" && !hasSwitchingEvents ? { ...g, hidden: true } : g,
  );

  // Filter events by active tags
  const filteredEvents = useMemo(() => {
    if (activeTags.size === 0) return events;
    return events.filter((se) => activeTags.has(se.event.event));
  }, [events, activeTags]);

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
        <span className="events-header__count">{filteredEvents.length} events</span>
      </div>

      {/* Filter groups */}
      <div className="events-filters">
        <button
          className={`events-filter-chip${activeTags.size === 0 ? " events-filter-chip--active" : ""}`}
          onClick={clearFilters}
        >
          all
        </button>
        {visibleGroups.map((g) =>
          g.hidden ? null : (
            <button
              key={g.id}
              className={`events-filter-chip${
                g.tags.some((t) => activeTags.has(t)) ? " events-filter-chip--active" : ""
              }`}
              onClick={() => {
                // Toggle all tags in the group
                const allActive = g.tags.every((t) => activeTags.has(t));
                for (const t of g.tags) {
                  if (allActive) {
                    if (activeTags.has(t)) toggleTag(t);
                  } else {
                    if (!activeTags.has(t)) toggleTag(t);
                  }
                }
              }}
            >
              {g.label}
            </button>
          ),
        )}
      </div>

      {showBanner && (
        <div className={`events-banner${lagged ? " events-banner--lag" : ""}`}>
          {bannerText}
        </div>
      )}

      <Card opaque className="events-card">
        {events.length === 0 ? (
          connected ? (
            <div className="events-empty">
              <div className="events-empty__text">Waiting for the first event…</div>
            </div>
          ) : (
            <div className="events-empty">Connecting to daemon…</div>
          )
        ) : filteredEvents.length === 0 && activeTags.size > 0 ? (
          <div className="events-empty">
            <span>No {[...activeTags].join(", ")} events in this session.</span>
            <button className="events-empty__clear" onClick={clearFilters}>
              clear filters
            </button>
          </div>
        ) : (
          <>
            {filteredEvents.map((se, i) => {
              // History separator sentinel — renders the divider.
              if ((se.event as { event: string }).event === "_history_separator") {
                return (
                  <div key="history-separator" className="events-separator">
                    — history —
                  </div>
                );
              }
              const tag = se.event.event;
              // Get badge info; unknown tags render via the default arm.
              let badge: { color: string; bg: string; label: string };
              try {
                badge = badgeForEvent(se.event as DaemonEvent);
              } catch {
                const raw = (se.event as { event: string }).event ?? "unknown";
                badge = { color: "var(--text-muted)", bg: "var(--bg-sunken)", label: raw };
              }
              const msg = messageForEvent(se.event as DaemonEvent);

              // Row borders for specific event kinds
              const isFailure = tag === "blank_failure" || tag === "config_reload_rejected";
              const isWearAdvisory = tag === "compensation_advisory";

              return (
                <div
                  key={`${se.time}-${i}`}
                  className={`events-row${isFailure ? " events-row--danger" : ""}${isWearAdvisory ? " events-row--warm" : ""}`}
                >
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
            })}
          </>
        )}
      </Card>
    </div>
  );
}

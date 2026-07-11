/**
 * Live-state provider — fetches initial daemon data and patches it
 * in-memory from incoming WebSocket events.
 *
 * A single `useEvents` subscription runs here so every view reads from
 * one connection.
 */
import { useState, useEffect, useCallback, useRef } from "react";
import type { ReactNode } from "react";
import { useEvents } from "../api/ws";
import { getState, getConfig } from "../api/client";
import type {
  SensorSnapshot,
  StateSnapshot,
  ConfigResponse,
  DaemonEvent,
  SensorConfig,
  ZoneConfig,
  DisplayConfig,
  DisplayRuleInfo,
  DisplaySnapshot,
} from "../api/types";
import {
  LiveStateContext,
  EventLogContext,
} from "./hooks/useLiveState";
import type {
  StampedEvent,
  LiveState,
  EventLogState,
  WearSnapshotPatch,
} from "./hooks/useLiveState";

const MAX_EVENTS = 100;

function formatTimestamp(): string {
  return new Date().toLocaleTimeString("en-GB", { hour12: false });
}

function patchSensorState(
  snapshot: StateSnapshot,
  sensor: string,
  state: string,
): StateSnapshot {
  const sensors = snapshot.sensors.map((s) =>
    s.id === sensor ? { ...s, state: state as SensorSnapshot["state"] } : s,
  );
  return { ...snapshot, sensors };
}

function patchZonePresence(
  snapshot: StateSnapshot,
  zone: string,
  present: boolean,
): StateSnapshot {
  const zones = snapshot.zones.map((z) =>
    z.id === zone ? { ...z, present } : z,
  );
  return { ...snapshot, zones };
}

function patchDisplayPhase(
  snapshot: StateSnapshot,
  display: string,
  phase: string,
): StateSnapshot {
  const displays = snapshot.displays.map(
    ([id, d]): [string, DisplaySnapshot] =>
      id === display
        ? [
            id,
            {
              ...d,
              phase,
              // Clear stage when leaving the staged phase; the WS event
              // carries no stage detail — a stale label would persist until
              // the next poll otherwise.
              stage: phase === "staged" ? d.stage : null,
            },
          ]
        : [id, d],
  );
  return { ...snapshot, displays };
}

export function LiveStateProvider({ children }: { children: ReactNode }) {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null);
  const [config, setConfig] = useState<ConfigResponse | null>(null);
  const [events, setEvents] = useState<StampedEvent[]>([]);
  const [lagged, setLagged] = useState(false);
  const [wearSnapshots, setWearSnapshots] = useState<Record<string, WearSnapshotPatch>>({});
  const [wearAdvisories, setWearAdvisories] = useState<Record<string, number>>({});
  const lagTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mountedRef = useRef(true);

  const fetchAll = useCallback(async () => {
    setError(null);
    try {
      const [snap, cfg] = await Promise.all([getState(), getConfig()]);
      if (!mountedRef.current) return;
      setSnapshot(snap);
      setConfig(cfg);
      setLoading(false);
    } catch (err: unknown) {
      if (!mountedRef.current) return;
      setError(err instanceof Error ? err.message : "Unknown error");
      setLoading(false);
    }
  }, []);

  const refresh = useCallback(() => {
    void fetchAll();
  }, [fetchAll]);

  useEffect(() => {
    mountedRef.current = true;
    void fetchAll();
    return () => {
      mountedRef.current = false;
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
    };
  }, [fetchAll]);

  const onMessage = useCallback((data: unknown) => {
    const ev = data as
      | (DaemonEvent & { skipped?: number })
      | ({ event: "stream_lagged"; skipped: number } & Record<string, unknown>);

    if (
      ev &&
      typeof ev === "object" &&
      "event" in ev &&
      ev.event === "stream_lagged"
    ) {
      setLagged(true);
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
      lagTimerRef.current = setTimeout(() => setLagged(false), 5_000);
      return;
    }

    if (ev && typeof ev === "object" && "event" in ev) {
      setEvents((prev) => {
        const next = [
          { time: formatTimestamp(), event: ev as DaemonEvent },
          ...prev,
        ];
        return next.length > MAX_EVENTS ? next.slice(0, MAX_EVENTS) : next;
      });

      const tag = (ev as { event: string }).event;

      setSnapshot((prev) => {
        if (!prev) return prev;
        switch (tag) {
          case "sensor_changed": {
            const se = ev as { sensor: string; state: string };
            return patchSensorState(prev, se.sensor, se.state);
          }
          case "zone_changed": {
            const ze = ev as { zone: string; present: boolean };
            return patchZonePresence(prev, ze.zone, ze.present);
          }
          case "display_phase": {
            const de = ev as { display: string; phase: string };
            return patchDisplayPhase(prev, de.display, de.phase);
          }
          case "wear_snapshot": {
            // Not part of StateSnapshot — patches the separate
            // wearSnapshots map as a side effect; the snapshot itself
            // is unchanged.
            const we = ev as {
              display: string;
              total_on_hours: number;
              sample_count: number;
            };
            setWearSnapshots((prevWear) => ({
              ...prevWear,
              [we.display]: {
                total_on_hours: we.total_on_hours,
                sample_count: we.sample_count,
              },
            }));
            return prev;
          }
          case "compensation_advisory": {
            // Best-effort nudge only — GET /api/wear remains the truth.
            const ce = ev as { display: string; hours_since_long_dwell: number };
            setWearAdvisories((prevAdv) => ({
              ...prevAdv,
              [ce.display]: ce.hours_since_long_dwell,
            }));
            return prev;
          }
          default:
            return prev;
        }
      });

      if (tag === "config_reloaded") {
        getConfig()
          .then((cfg) => {
            if (mountedRef.current) setConfig(cfg);
          })
          .catch(() => {});
      }
    }
  }, []);

  const { connected } = useEvents({ onMessage, onConnect: () => { void fetchAll(); } });

  const sensorConfigs: Record<string, SensorConfig> = {};
  const zoneConfigs: Record<string, ZoneConfig> = {};
  const displayConfigs: Record<string, DisplayConfig> = {};
  const displayRules: Record<string, DisplayRuleInfo> = {};

  if (config) {
    const inv = config.inventory;
    if (inv.sensors) Object.assign(sensorConfigs, inv.sensors);
    if (inv.zones) Object.assign(zoneConfigs, inv.zones);
    if (inv.displays) Object.assign(displayConfigs, inv.displays);
    if (config.display_rules) Object.assign(displayRules, config.display_rules);
  }

  const liveState: LiveState = {
    loading,
    error,
    snapshot,
    config,
    connected,
    sensorConfigs,
    zoneConfigs,
    displayConfigs,
    displayRules,
    wearSnapshots,
    wearAdvisories,
    refresh,
  };

  const eventLog: EventLogState = { events, connected, lagged };

  return (
    <LiveStateContext.Provider value={liveState}>
      <EventLogContext.Provider value={eventLog}>
        {children}
      </EventLogContext.Provider>
    </LiveStateContext.Provider>
  );
}

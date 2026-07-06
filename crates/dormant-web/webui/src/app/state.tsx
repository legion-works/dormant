/**
 * Live state — React context that fetches initial daemon data and
 * patches it in-memory from incoming WebSocket events.
 *
 * A single `useEvents` subscription runs at the provider level so every
 * view reads from one connection.  Patch logic:
 *   sensor_changed → update that sensor's state
 *   zone_changed   → update that zone's presence
 *   display_phase  → update that display's phase
 *   config_reloaded → re-fetch /api/config
 *   wake_retry     → no state mutation (event-log only)
 *
 * The Events view consumes the event log via `useEventLog()`; the
 * Dashboard and Displays views consume the live snapshot + config maps
 * via `useLiveState()`.
 */
import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  useRef,
} from "react";
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

// ── Event log entry (after serialisation) ───────────────────────────────

const MAX_EVENTS = 100;

export interface StampedEvent {
  /** ISO time string captured at arrival. */
  time: string;
  event: DaemonEvent;
}

// ── Live-state shape ────────────────────────────────────────────────────

export interface LiveState {
  loading: boolean;
  error: string | null;
  snapshot: StateSnapshot | null;
  config: ConfigResponse | null;
  connected: boolean;
  sensorConfigs: Record<string, SensorConfig>;
  zoneConfigs: Record<string, ZoneConfig>;
  displayConfigs: Record<string, DisplayConfig>;
  displayRules: Record<string, DisplayRuleInfo>;
  refresh: () => void;
}

const LiveStateContext = createContext<LiveState | null>(null);

export interface EventLogState {
  events: StampedEvent[];
  connected: boolean;
  lagged: boolean;
}

const EventLogContext = createContext<EventLogState | null>(null);

// ── Helpers ─────────────────────────────────────────────────────────────

function formatTimestamp(): string {
  return new Date().toLocaleTimeString("en-GB", { hour12: false });
}

/** Deep-clone a snapshot and patch one sensor's state in place. */
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

/** Deep-clone a snapshot and patch one zone's presence in place. */
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

/** Deep-clone a snapshot and patch one display's phase in place. */
function patchDisplayPhase(
  snapshot: StateSnapshot,
  display: string,
  phase: string,
): StateSnapshot {
  const displays = snapshot.displays.map(
    ([id, d]): [string, DisplaySnapshot] =>
      id === display ? [id, { ...d, phase }] : [id, d],
  );
  return { ...snapshot, displays };
}

// ── Provider ────────────────────────────────────────────────────────────

export function LiveStateProvider({ children }: { children: ReactNode }) {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null);
  const [config, setConfig] = useState<ConfigResponse | null>(null);
  const [events, setEvents] = useState<StampedEvent[]>([]);
  const [lagged, setLagged] = useState(false);
  const lagTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mountedRef = useRef(true);

  // Keep callback refs so useEvents' onMessage closure stays stable.
  const refreshRef = useRef<() => void>(() => {});

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

  refreshRef.current = refresh;

  // Fetch initial state on mount.
  useEffect(() => {
    mountedRef.current = true;
    void fetchAll();
    return () => {
      mountedRef.current = false;
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
    };
  }, [fetchAll]);

  // Patch state from incoming DaemonEvents — callback identity is
  // stable because we use refs for the setters.
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
      // Surface the lag banner via the event-log context.
      setLagged(true);
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
      lagTimerRef.current = setTimeout(() => setLagged(false), 5_000);
      return;
    }

    if (ev && typeof ev === "object" && "event" in ev) {
      // Append to event log.
      setEvents((prev) => {
        const next = [
          { time: formatTimestamp(), event: ev as DaemonEvent },
          ...prev,
        ];
        return next.length > MAX_EVENTS ? next.slice(0, MAX_EVENTS) : next;
      });

      const tag = (ev as { event: string }).event;

      // Patch in-memory snapshot.
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
          default:
            return prev;
        }
      });

      // Re-fetch config on reload so inventory + display_rules stay fresh.
      if (tag === "config_reloaded") {
        getConfig()
          .then((cfg) => {
            if (mountedRef.current) setConfig(cfg);
          })
          .catch(() => {
            // Config fetch may fail if the new config is invalid;
            // the old config data stays visible.
          });
      }
    }
  }, []);

  const { connected } = useEvents({ onMessage });

  // Build lookup maps from config inventory.
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

// ── Hooks ───────────────────────────────────────────────────────────────

/** Read live state (for Dashboard, Displays). */
export function useLiveState(): LiveState {
  const ctx = useContext(LiveStateContext);
  if (!ctx) {
    throw new Error("useLiveState must be used within LiveStateProvider");
  }
  return ctx;
}

/** Read the event log (for Events view). */
export function useEventLog(): EventLogState {
  const ctx = useContext(EventLogContext);
  if (!ctx) {
    throw new Error("useEventLog must be used within LiveStateProvider");
  }
  return ctx;
}

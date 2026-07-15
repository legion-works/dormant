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
import { getState, getConfig, getOperations, getWear, getWearDetail } from "../api/client";
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
  OperationsStatus,
  WearDetail,
  WearListResponse,
  DoctorReport,
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

/** Patch a display's `wake_attempts` counter (from `wake_retry`/`wake_recovered`). */
function patchWakeAttempts(
  snapshot: StateSnapshot,
  display: string,
  attempts: number,
): StateSnapshot {
  const displays = snapshot.displays.map(
    ([id, d]): [string, DisplaySnapshot] =>
      id === display ? [id, { ...d, wake_attempts: attempts }] : [id, d],
  );
  return { ...snapshot, displays };
}

/** Patch a display's `last_blank_failed` flag (from `blank_failure`/`blank_recovered`). */
function patchBlankFailed(
  snapshot: StateSnapshot,
  display: string,
  failed: boolean,
): StateSnapshot {
  const displays = snapshot.displays.map(
    ([id, d]): [string, DisplaySnapshot] =>
      id === display ? [id, { ...d, last_blank_failed: failed }] : [id, d],
  );
  return { ...snapshot, displays };
}

type FetchKind = "initial" | "background";

function rejectionMessage(result: PromiseRejectedResult): string {
  return result.reason instanceof Error ? result.reason.message : "Unknown error";
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

  const [wear, setWear] = useState<WearListResponse | null>(null);
  const [wearDetails, setWearDetails] = useState<Record<string, WearDetail>>({});
  const [wearError, setWearError] = useState<string | null>(null);
  interface OperationsObservation {
    requestId: number;
    status: OperationsStatus;
  }

  const [operationsObservation, setOperationsObservation] =
    useState<OperationsObservation | null>(null);
  const operations = operationsObservation?.status ?? null;
  const operationsRequestId = operationsObservation?.requestId ?? 0;
  const [statePollWarning, setStatePollWarning] = useState<string | null>(null);
  const [backgroundRefreshWarning, setBackgroundRefreshWarning] = useState<string | null>(null);
  const pollWarning = backgroundRefreshWarning ?? statePollWarning;
  const [selectedDisplay, selectDisplay] = useState<string | null>(null);
  const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
  const wearRequestSequence = useRef(0);
  const operationsRequestSequence = useRef(0);

  const commitOperations = useCallback((status: OperationsStatus, requestId: number) => {
    setOperationsObservation((current) =>
      current && current.requestId > requestId ? current : { requestId, status },
    );
  }, []);

  const refreshOperations = useCallback(async (
    onStart?: (requestId: number) => void,
  ): Promise<OperationsStatus> => {
    const requestId = ++operationsRequestSequence.current;
    onStart?.(requestId);
    const status = await getOperations();
    if (mountedRef.current) commitOperations(status, requestId);
    return status;
  }, [commitOperations]);

  const refreshWear = useCallback(async () => {
    const request = ++wearRequestSequence.current;
    try {
      const list = await getWear();
      const settled = await Promise.allSettled(
        list.displays.map(async ({ display }) => getWearDetail(display)),
      );
      if (!mountedRef.current || request !== wearRequestSequence.current) return;
      const details = settled.flatMap((result) =>
        result.status === "fulfilled" ? [result.value] : [],
      );
      const failures = settled.length - details.length;
      setWear(list);
      // Replacing, rather than merging, invalidates removed displays and stale ids.
      setWearDetails(Object.fromEntries(details.map((detail) => [detail.display_name, detail])));
      setWearError(failures > 0 ? `${failures} wear detail request${failures === 1 ? "" : "s"} failed` : null);
    } catch (err: unknown) {
      if (!mountedRef.current || request !== wearRequestSequence.current) return;
      setWearError(err instanceof Error ? err.message : "Wear data unavailable");
    }
  }, []);

  const fetchAll = useCallback(async (kind: FetchKind) => {
    if (kind === "initial") {
      setLoading(true);
      setError(null);
    }

    const operationsPromise = refreshOperations();
    const [snapshotResult, configResult, operationsResult] = await Promise.allSettled([
      getState(),
      getConfig(),
      operationsPromise,
    ]);
    if (!mountedRef.current) return;

    const requiredFailures = [snapshotResult, configResult].filter(
      (result): result is PromiseRejectedResult => result.status === "rejected",
    );
    if (kind === "initial" && requiredFailures.length > 0) {
      setError(requiredFailures.map(rejectionMessage).join("; "));
      setLoading(false);
      if (operationsResult.status === "rejected") {
        setStatePollWarning(rejectionMessage(operationsResult));
      }
      return;
    }

    if (snapshotResult.status === "fulfilled") setSnapshot(snapshotResult.value);
    if (configResult.status === "fulfilled") setConfig(configResult.value);
    const failures = [snapshotResult, configResult, operationsResult].filter(
      (result): result is PromiseRejectedResult => result.status === "rejected",
    );
    if (kind === "background") {
      setBackgroundRefreshWarning(
        failures.length > 0 ? failures.map(rejectionMessage).join("; ") : null,
      );
    } else {
      setError(null);
      setLoading(false);
      setStatePollWarning(
        operationsResult.status === "rejected" ? rejectionMessage(operationsResult) : null,
      );
    }
  }, [refreshOperations]);

  const refresh = useCallback(() => {
    void fetchAll("background");
  }, [fetchAll]);

  useEffect(() => {
    mountedRef.current = true;
    void fetchAll("initial");
    void refreshWear();
    return () => {
      mountedRef.current = false;
      if (lagTimerRef.current != null) clearTimeout(lagTimerRef.current);
    };
  }, [fetchAll, refreshWear]);

  // Poll state and authoritative operation guards together at one-second
  // cadence. `error` remains reserved for the fatal initial getState/getConfig
  // load — a status-poll failure leaves the last snapshot and operation
  // status usable and writes only `statePollWarning`.
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const poll = async () => {
      const operationsPromise = refreshOperations();
      const [snapshotResult, operationsResult] = await Promise.allSettled([
        getState(),
        operationsPromise,
      ]);
      if (cancelled) return;
      if (snapshotResult.status === "fulfilled") setSnapshot(snapshotResult.value);
      const failures = [snapshotResult, operationsResult].filter(
        (result): result is PromiseRejectedResult => result.status === "rejected",
      );
      setStatePollWarning(
        failures.length > 0 ? failures.map(rejectionMessage).join("; ") : null,
      );
      timer = setTimeout(poll, 1000);
    };

    timer = setTimeout(poll, 1000);
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [refreshOperations]);

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
          case "wake_retry": {
            const we = ev as { display: string; attempt: number };
            return patchWakeAttempts(prev, we.display, we.attempt);
          }
          case "wake_recovered": {
            const re = ev as { display: string; attempts: number };
            return patchWakeAttempts(prev, re.display, 0);
          }
          case "blank_failure": {
            const be = ev as { display: string };
            return patchBlankFailed(prev, be.display, true);
          }
          case "blank_recovered": {
            const be = ev as { display: string };
            return patchBlankFailed(prev, be.display, false);
          }
          case "wear_snapshot": {
            // Not part of StateSnapshot — patches the separate
            // wearSnapshots map as a side effect; the snapshot itself
            // is unchanged. Also kicks off an authoritative refresh so
            // GET /api/wear catches up promptly.
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
            void refreshWear();
            return prev;
          }
          case "compensation_advisory": {
            // Best-effort nudge only — GET /api/wear remains the truth.
            const ce = ev as { display: string; hours_since_long_dwell: number };
            setWearAdvisories((prevAdv) => ({
              ...prevAdv,
              [ce.display]: ce.hours_since_long_dwell,
            }));
            void refreshWear();
            return prev;
          }
          default:
            return prev;
        }
      });

      if (tag === "config_reloaded") {
        void refresh();
        void refreshWear();
      }
    }
  }, [refresh, refreshWear]);

  const onConnect = useCallback(() => {
    refresh();
  }, [refresh]);

  const { connected } = useEvents({ onMessage, onConnect });

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
    pollWarning,
    snapshot,
    config,
    connected,
    sensorConfigs,
    zoneConfigs,
    displayConfigs,
    displayRules,
    wearSnapshots,
    wearAdvisories,
    operations,
    operationsRequestId,
    refreshOperations,
    wear,
    wearDetails,
    wearError,
    selectedDisplay,
    selectDisplay,
    refreshWear,
    doctorReport,
    setDoctorReport,
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

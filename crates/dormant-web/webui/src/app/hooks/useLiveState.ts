/**
 * Shared live-state contexts and hooks.
 *
 * [`LiveStateContext`] and [`EventLogContext`] are consumed by
 * [`useLiveState`] and [`useEventLog`]; the provider lives in
 * [`../state`]`::LiveStateProvider` so the component file exports
 * only the component (no hook exports — keeps oxlint's
 * `only-export-components` happy).
 */
import { createContext, useContext } from "react";
import type {
  StateSnapshot,
  ConfigResponse,
  DaemonEvent,
  SensorConfig,
  ZoneConfig,
  DisplayConfig,
  DisplayRuleInfo,
} from "../../api/types";

/** Live-nudged numbers from `wear_snapshot` WS events, keyed by the wear
 * tracker's storage key (not necessarily the config display id). These
 * patch the numbers a fetched `WearSummary` displays; the fetch itself
 * (`GET /api/wear`) remains the source of truth on mount/refresh. */
export interface WearSnapshotPatch {
  total_on_hours: number;
  sample_count: number;
}

export interface StampedEvent {
  /** ISO time string captured at arrival. */
  time: string;
  event: DaemonEvent;
}

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
  /** `wear_snapshot` WS nudges, keyed by wear-tracker storage key. */
  wearSnapshots: Record<string, WearSnapshotPatch>;
  /** `compensation_advisory` WS nudges: hours since last long-dwell,
   * keyed by wear-tracker storage key. A fresh `GET /api/wear` fetch
   * remains authoritative — this is a best-effort UI nudge only. */
  wearAdvisories: Record<string, number>;
  refresh: () => void;
}

export const LiveStateContext = createContext<LiveState | null>(null);

export interface EventLogState {
  events: StampedEvent[];
  connected: boolean;
  lagged: boolean;
}

export const EventLogContext = createContext<EventLogState | null>(null);

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

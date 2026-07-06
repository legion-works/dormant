/**
 * Shared data-fetching hook for Dashboard and Displays views.
 *
 * Fetches StateSnapshot + ConfigResponse on mount and exposes the
 * raw data plus convenience lookup maps (sensor type, zone mode,
 * display blank_mode, display→zone→rule reverse mapping).
 *
 * Both views consume the same two endpoints; this hook avoids
 * duplicate fetch logic.
 */
import { useState, useEffect, useCallback, useRef } from "react";
import type { StateSnapshot, ConfigResponse, SensorConfig, ZoneConfig, DisplayConfig, DisplayRuleInfo } from "../../api/types";
import { getState, getConfig } from "../../api/client";

export interface DashboardData {
  loading: boolean;
  error: string | null;
  snapshot: StateSnapshot | null;
  config: ConfigResponse | null;
  sensorConfigs: Record<string, SensorConfig>;
  zoneConfigs: Record<string, ZoneConfig>;
  displayConfigs: Record<string, DisplayConfig>;
  displayRules: Record<string, DisplayRuleInfo>;
  refresh: () => void;
}

export function useDashboardData(): DashboardData {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null);
  const [config, setConfig] = useState<ConfigResponse | null>(null);
  const mountedRef = useRef(true);

  const fetchAll = useCallback(async () => {
    // Don't show full loading spinner on refresh — only on initial load.
    setError(null);

    try {
      const [snap, cfg] = await Promise.all([getState(), getConfig()]);
      if (!mountedRef.current) return;
      setSnapshot(snap);
      setConfig(cfg);
      setLoading(false);
    } catch (err: unknown) {
      if (!mountedRef.current) return;
      const msg = err instanceof Error ? err.message : "Unknown error";
      setError(msg);
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
    };
  }, [fetchAll]);

  // Build lookup maps from config inventory for convenience.
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

  return {
    loading,
    error,
    snapshot,
    config,
    sensorConfigs,
    zoneConfigs,
    displayConfigs,
    displayRules,
    refresh,
  };
}

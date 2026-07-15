import type { LiveState, EventLogState } from "../../app/hooks/useLiveState";

export function liveStateFixture(overrides: Partial<LiveState> = {}): LiveState {
  return {
    loading: false,
    error: null,
    pollWarning: null,
    snapshot: { sensors: [], zones: [], displays: [], pending_reload: null },
    config: null,
    connected: true,
    sensorConfigs: {},
    zoneConfigs: {},
    displayConfigs: {},
    displayRules: {},
    wearSnapshots: {},
    wearAdvisories: {},
    operations: { exercise_in_flight: [], emergency_wake_in_flight: false },
    operationsRequestId: 1,
    refreshOperations: async (onStart) => {
      onStart?.(2);
      return {
        exercise_in_flight: [],
        emergency_wake_in_flight: false,
      };
    },
    wear: { displays: [] },
    wearDetails: {},
    wearError: null,
    selectedDisplay: null,
    selectDisplay: () => undefined,
    refreshWear: async () => undefined,
    doctorReport: null,
    setDoctorReport: () => undefined,
    refresh: () => undefined,
    ...overrides,
  };
}

export function eventLogFixture(overrides: Partial<EventLogState> = {}): EventLogState {
  return { events: [], connected: true, lagged: false, ...overrides };
}

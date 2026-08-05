/**
 * Live-state provider tests — event→state patching through the real
 * LiveStateProvider with a mocked WebSocket hook.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, act } from "@testing-library/react";
import { LiveStateProvider } from "../app/state";
import { useLiveState, useEventLog } from "../app/hooks/useLiveState";
import type { StateSnapshot, DisplaySnapshot, WearListResponse } from "../api/types";


const { mocks, fixtures } = vi.hoisted(() => {
  let capturedOnMessage: ((data: unknown) => void) | null = null;
  let capturedOnConnect: (() => void) | null = null;

  const useEventsImpl = vi.fn(
    (opts: { onMessage: (data: unknown) => void; onConnect?: () => void }) => {
      capturedOnMessage = opts.onMessage;
      if (opts.onConnect) capturedOnConnect = opts.onConnect;
      return { connected: true, close: vi.fn() };
    },
  );

  const state = {
    sensors: [
      { id: "s1", state: "absent" as const, last_seen_secs_ago: 10 },
      { id: "s2", state: "present" as const, last_seen_secs_ago: 2 },
    ],
    zones: [
      { id: "z1", present: false },
      { id: "z2", present: true },
    ],
    displays: [
      [
        "d1",
        { phase: "active", inhibited: false, paused: false, cmd_gen: 1, controllers: [] },
      ],
    ],
    pending_reload: null,
  };

  const config = {
    path: "/tmp/c.toml",
    config_version: 1,
    source: "last_applied" as const,
    raw_toml: "",
    inventory: {
      config_version: 1,
      daemon: {},
      sensors: {
        s1: { type: "usb-ld2410" as const, port: "/dev/ttyX" },
        s2: { type: "mqtt" as const, broker_url: "", topic: "" },
      },
      zones: {
        z1: { mode: "any", members: ["s1"], weights: {}, unavailable_policy: "present" as const },
        z2: { mode: "all", members: ["s2"], weights: {}, unavailable_policy: "absent" as const },
      },
      displays: { d1: { controllers: ["ddcci"], blank_mode: "power_off" as const } },
      rules: {},
    },
    validation: { ok: true, warnings: [], errors: [] },
    display_rules: {},
  };

  return {
    mocks: {
      useEventsImpl,
      get onMessage() { return capturedOnMessage; },
      get onConnect() { return capturedOnConnect; },
    },
    fixtures: { state, config },
  };
});

vi.mock("../api/ws", () => ({
  useEvents: mocks.useEventsImpl,
}));

vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(fixtures.state),
  getConfig: vi.fn().mockResolvedValue(fixtures.config),
  getWear: vi.fn().mockResolvedValue({ displays: [] }),
  getWearDetail: vi.fn().mockRejectedValue(new Error("unexpected wear detail request")),
  getOperations: vi.fn().mockResolvedValue({
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  }),
}));

afterEach(async () => {
  cleanup();
  vi.clearAllMocks();
  vi.restoreAllMocks();
  // Reset getOperations so that mockReturnValue from an in-flight-HTTP test
  // (which returns a never-resolving promise) does not bleed into the next test.
  const { getOperations } = await import("../api/client");
  vi.mocked(getOperations).mockResolvedValue({
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  });
});

function SensorConsumer() {
  const { snapshot } = useLiveState();
  if (!snapshot) return <span>loading</span>;
  return (
    <div>
      {snapshot.sensors.map((s) => (
        <span key={s.id} data-testid={`sensor-${s.id}`}>
          {s.state}
        </span>
      ))}
    </div>
  );
}

describe("LiveStateProvider event-to-state patching", () => {
  it("loads initial state from mocked API", async () => {
    render(
      <LiveStateProvider>
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });
    expect(screen.getByTestId("sensor-s2")).toHaveTextContent("present");
  });

  it("sensor_changed patches the correct sensor state", async () => {
    render(
      <LiveStateProvider>
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });

    act(() => {
      mocks.onMessage?.({ event: "sensor_changed", sensor: "s1", state: "present" });
    });

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("present");
    });
    expect(screen.getByTestId("sensor-s2")).toHaveTextContent("present");
  });

  it("zone_changed patches the correct zone presence", async () => {
    function ZoneConsumer() {
      const { snapshot } = useLiveState();
      if (!snapshot) return <span>loading</span>;
      return (
        <div>
          {snapshot.zones.map((z) => (
            <span key={z.id} data-testid={`zone-${z.id}`}>
              {String(z.present)}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <ZoneConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("zone-z1")).toHaveTextContent("false");
    });

    act(() => {
      mocks.onMessage?.({ event: "zone_changed", zone: "z1", present: true, cause: "s1" });
    });

    await waitFor(() => {
      expect(screen.getByTestId("zone-z1")).toHaveTextContent("true");
    });
  });

  it("display_phase patches the correct display phase", async () => {
    function DisplayConsumer() {
      const { snapshot } = useLiveState();
      if (!snapshot) return <span>loading</span>;
      return (
        <div>
          {snapshot.displays.map(([id, d]) => (
            <span key={id} data-testid={`display-${id}`}>
              {d.phase}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <DisplayConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("display-d1")).toHaveTextContent("active");
    });

    act(() => {
      mocks.onMessage?.({ event: "display_phase", display: "d1", phase: "blanking", cause: "x" });
    });

    await waitFor(() => {
      expect(screen.getByTestId("display-d1")).toHaveTextContent("blanking");
    });
  });

  it("event log accumulates events", async () => {
    function EventConsumer() {
      const { events } = useEventLog();
      return (
        <div>
          <span data-testid="ev-count">{events.length}</span>
          {events.map((e, i) => (
            <span key={i} data-testid={`ev-${i}`}>
              {e.event.event}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <EventConsumer />
      </LiveStateProvider>,
    );

    act(() => {
      mocks.onMessage?.({ event: "sensor_changed", sensor: "a", state: "present" });
      mocks.onMessage?.({ event: "zone_changed", zone: "z", present: true, cause: "a" });
    });

    await waitFor(() => {
      expect(screen.getByTestId("ev-count")).toHaveTextContent("2");
    });
    expect(screen.getByTestId("ev-0")).toHaveTextContent("zone_changed");
    expect(screen.getByTestId("ev-1")).toHaveTextContent("sensor_changed");
  });

  it("config_reloaded triggers config refetch", async () => {
    const { getConfig } = await import("../api/client");

    // Initial fetch counts: 1 call from LiveStateProvider's useEffect.
    render(
      <LiveStateProvider>
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });

    const callsBefore = vi.mocked(getConfig).mock.calls.length;

    act(() => {
      mocks.onMessage?.({ event: "config_reloaded" });
    });

    // The provider should call getConfig again after config_reloaded.
    await waitFor(() => {
      expect(vi.mocked(getConfig).mock.calls.length).toBeGreaterThan(callsBefore);
    });
  });

  it("onConnect triggers state+config refetch", async () => {
    const { getState, getConfig, getOperations } = await import("../api/client");

    render(
      <LiveStateProvider>
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });

    const stateCallsBefore = vi.mocked(getState).mock.calls.length;
    const configCallsBefore = vi.mocked(getConfig).mock.calls.length;
    const opsCallsBefore = vi.mocked(getOperations).mock.calls.length;

    // Simulate a WS reconnect — should trigger a full refetch.
    act(() => {
      mocks.onConnect?.();
    });

    await waitFor(() => {
      expect(vi.mocked(getState).mock.calls.length).toBeGreaterThan(stateCallsBefore);
    });
    expect(vi.mocked(getConfig).mock.calls.length).toBeGreaterThan(configCallsBefore);
    expect(vi.mocked(getOperations).mock.calls.length).toBeGreaterThan(opsCallsBefore);
  });
});

  it("clears stage when display phase patches away from staged", async () => {
    const stagedSnapshot: StateSnapshot = {
      ...fixtures.state,
      displays: [
        [
          "d1" as string,
          {
            phase: "staged",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            stage: { idx: 2, kind: "render_black" },
          } as DisplaySnapshot,
        ] as [string, DisplaySnapshot],
      ],
    };

    // Override getState to return the staged fixture.
    // eslint-disable-next-line @typescript-eslint/no-unsafe-assignment
    const { getState } = await import("../api/client");
    vi.mocked(getState).mockResolvedValue(stagedSnapshot);

    function StageConsumer() {
      const { snapshot } = useLiveState();
      if (!snapshot) return <span>loading</span>;
      return (
        <div>
          {snapshot.displays.map(([id, d]) => (
            <span key={id} data-testid={`display-${id}`}>
              {d.phase}:{d.stage?.kind ?? "none"}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <StageConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("display-d1")).toHaveTextContent("staged:render_black");
    });

    // Phase changes away from staged — stage must clear.
    act(() => {
      mocks.onMessage?.({
        event: "display_phase",
        display: "d1",
        phase: "blanking",
        cause: "z",
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("display-d1")).toHaveTextContent("blanking:none");
    });
  });

  it("stage stays intact during staged→staged phase patch", async () => {
    const stagedSnapshot: StateSnapshot = {
      ...fixtures.state,
      displays: [
        [
          "d1" as string,
          {
            phase: "staged",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            stage: { idx: 1, kind: "render_screensaver" },
          } as DisplaySnapshot,
        ] as [string, DisplaySnapshot],
      ],
    };

    const { getState } = await import("../api/client");
    vi.mocked(getState).mockResolvedValue(stagedSnapshot);

    function StageConsumer() {
      const { snapshot } = useLiveState();
      if (!snapshot) return <span>loading</span>;
      return (
        <div>
          {snapshot.displays.map(([id, d]) => (
            <span key={id} data-testid={`display-${id}`}>
              {d.phase}:{d.stage?.kind ?? "none"}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <StageConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("display-d1")).toHaveTextContent(
        "staged:render_screensaver",
      );
    });

    // Same phase, different "reason" — stage must survive.
    act(() => {
      mocks.onMessage?.({
        event: "display_phase",
        display: "d1",
        phase: "staged",
        cause: "ladder advance",
      });
    });

    await waitFor(() => {
      // stage.kind should still be render_screensaver (from the fixture)
      expect(screen.getByTestId("display-d1")).toHaveTextContent(
        "staged:render_screensaver",
      );
    });
  });

describe("LiveStateProvider wear events", () => {
  function WearConsumer() {
    const { wearSnapshots, wearAdvisories, snapshot } = useLiveState();
    if (!snapshot) return <span>loading</span>;
    return (
      <div>
        <span data-testid="wear-snap-d1">
          {wearSnapshots.d1
            ? `${wearSnapshots.d1.total_on_hours}:${wearSnapshots.d1.sample_count}`
            : "none"}
        </span>
        <span data-testid="wear-adv-d1">
          {wearAdvisories.d1 !== undefined ? String(wearAdvisories.d1) : "none"}
        </span>
      </div>
    );
  }

  it("wear_snapshot patches the wearSnapshots map without touching the StateSnapshot", async () => {
    render(
      <LiveStateProvider>
        <WearConsumer />
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });
    expect(screen.getByTestId("wear-snap-d1")).toHaveTextContent("none");

    act(() => {
      mocks.onMessage?.({
        event: "wear_snapshot",
        display: "d1",
        total_on_hours: 42.5,
        sample_count: 10,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("wear-snap-d1")).toHaveTextContent("42.5:10");
    });
    // The StateSnapshot itself is untouched by a wear_snapshot event.
    expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
  });

  it("compensation_advisory patches the wearAdvisories map (best-effort nudge)", async () => {
    render(
      <LiveStateProvider>
        <WearConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("wear-adv-d1")).toHaveTextContent("none");
    });

    act(() => {
      mocks.onMessage?.({
        event: "compensation_advisory",
        display: "d1",
        hours_since_long_dwell: 100,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("wear-adv-d1")).toHaveTextContent("100");
    });
  });

  it("an unknown WS event tag is a no-op — snapshot, wearSnapshots, and wearAdvisories all unchanged", async () => {
    render(
      <LiveStateProvider>
        <WearConsumer />
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });

    act(() => {
      mocks.onMessage?.({ event: "some_future_tag", display: "d1", whatever: 1 });
    });

    // Give any (incorrect) handler a chance to run, then assert nothing moved.
    await new Promise((r) => setTimeout(r, 10));
    expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    expect(screen.getByTestId("wear-snap-d1")).toHaveTextContent("none");
    expect(screen.getByTestId("wear-adv-d1")).toHaveTextContent("none");
  });
});

describe("LiveStateProvider wake/blank failure events", () => {
  function FailureConsumer() {
    const { snapshot } = useLiveState();
    if (!snapshot) return <span>loading</span>;
    return (
      <div>
        {snapshot.displays.map(([id, d]) => (
          <span key={id} data-testid={`failure-${id}`}>
            {String(d.wake_attempts ?? "undef")}:{String(d.last_blank_failed ?? "undef")}
          </span>
        ))}
      </div>
    );
  }

  it("wake_retry patches wake_attempts to the event's attempt count", async () => {
    render(
      <LiveStateProvider>
        <FailureConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:undef");
    });

    act(() => {
      mocks.onMessage?.({ event: "wake_retry", display: "d1", attempt: 2 });
    });

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("2:undef");
    });
  });

  it("wake_recovered resets wake_attempts to 0", async () => {
    render(
      <LiveStateProvider>
        <FailureConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:undef");
    });

    act(() => {
      mocks.onMessage?.({ event: "wake_retry", display: "d1", attempt: 3 });
    });
    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("3:undef");
    });

    act(() => {
      mocks.onMessage?.({ event: "wake_recovered", display: "d1", attempts: 3 });
    });

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("0:undef");
    });
  });

  it("blank_failure sets last_blank_failed to true", async () => {
    render(
      <LiveStateProvider>
        <FailureConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:undef");
    });

    act(() => {
      mocks.onMessage?.({
        event: "blank_failure",
        display: "d1",
        controller: "ddcci",
        detail: "E_TIMEOUT: no ack",
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:true");
    });
  });

  it("blank_recovered sets last_blank_failed to false", async () => {
    render(
      <LiveStateProvider>
        <FailureConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:undef");
    });

    act(() => {
      mocks.onMessage?.({
        event: "blank_failure",
        display: "d1",
        controller: "ddcci",
        detail: "E_TIMEOUT: no ack",
      });
    });
    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:true");
    });

    act(() => {
      mocks.onMessage?.({ event: "blank_recovered", display: "d1" });
    });

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:false");
    });
  });

  it("an unknown WS event tag is a no-op — displays unchanged", async () => {
    render(
      <LiveStateProvider>
        <FailureConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:undef");
    });

    act(() => {
      mocks.onMessage?.({ event: "some_future_tag", display: "d1", whatever: 1 });
    });

    await new Promise((r) => setTimeout(r, 10));
    expect(screen.getByTestId("failure-d1")).toHaveTextContent("undef:undef");
  });
});

describe("LiveStateProvider ownership events", () => {
  function ObservedInputConsumer() {
    const { snapshot } = useLiveState();
    if (!snapshot) return <span>loading</span>;
    const code = snapshot.displays.find(([id]) => id === "d1")?.[1].observed_input_code;
    return <span data-testid="observed-input">{String(code ?? "null")}</span>;
  }

  it("#200 explicit null observed_input_code clears the stored value (does not retain prior)", async () => {
    const priorState: StateSnapshot = {
      ...fixtures.state,
      displays: [["d1", {
        phase: "active",
        inhibited: false,
        paused: false,
        cmd_gen: 1,
        controllers: [],
        owned: true,
        observed_input_code: 0x10 as number | null,
      }]] as StateSnapshot["displays"],
    };
    const { getState } = await import("../api/client");
    vi.mocked(getState).mockResolvedValue(priorState);

    render(
      <LiveStateProvider>
        <ObservedInputConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("observed-input")).toHaveTextContent("16");
    });

    // Send an ownership event where observed_input_code is explicitly null
    // (input became unreadable) — the stored value MUST become null, NOT 0x10.
    act(() => {
      mocks.onMessage?.({
        event: "ownership",
        display: "d1",
        owned: true,
        observed_input_code: null,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("observed-input")).toHaveTextContent("null");
    });
  });

  it("#200 observed_input_code: undefined in event also clears (cannot occur on real wire — null is the wire unreadable value; this tests the defensive path)", async () => {
    const priorState = {
      ...fixtures.state,
      displays: [["d1", {
        phase: "active",
        inhibited: false,
        paused: false,
        cmd_gen: 1,
        controllers: [],
        owned: true,
        observed_input_code: 0x10 as number | null,
      }]] as StateSnapshot["displays"],
    };
    const { getState } = await import("../api/client");
    vi.mocked(getState).mockResolvedValue(priorState);

    render(
      <LiveStateProvider>
        <ObservedInputConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("observed-input")).toHaveTextContent("16");
    });

    // Send observed_input_code: undefined (key IS present, value is undefined).
    // This is distinct from absent (key not present) — the in check handles each correctly.
    act(() => {
      mocks.onMessage?.({
        event: "ownership",
        display: "d1",
        owned: true,
        observed_input_code: undefined,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("observed-input")).toHaveTextContent("null");
    });
  });
});

// ── issue #184: operations_changed (push, not 1 Hz poll) ───────────────────

describe("LiveStateProvider operations_changed event (issue #184)", () => {

  it("operations_changed event drives operations state without polling", async () => {
    function OperationsConsumer() {
      const { operations } = useLiveState();
      if (!operations) return <span>loading</span>;
      return (
        <div>
          <span data-testid="ops-exercise">{operations.exercise_in_flight.join(",")}</span>
          <span data-testid="ops-emergency">{String(operations.emergency_wake_in_flight)}</span>
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <OperationsConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("ops-exercise")).toHaveTextContent("");
    });
    expect(screen.getByTestId("ops-emergency")).toHaveTextContent("false");

    // Drive an exercise-in-flight event
    act(() => {
      mocks.onMessage?.({
        event: "operations_changed",
        exercise_in_flight: ["main"],
        emergency_wake_in_flight: false,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("ops-exercise")).toHaveTextContent("main");
    });
    expect(screen.getByTestId("ops-emergency")).toHaveTextContent("false");

    // Drive an emergency-wake-in-flight event
    act(() => {
      mocks.onMessage?.({
        event: "operations_changed",
        exercise_in_flight: ["main"],
        emergency_wake_in_flight: true,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("ops-emergency")).toHaveTextContent("true");
    });
    expect(screen.getByTestId("ops-exercise")).toHaveTextContent("main");

    // Drive completion (both cleared)
    act(() => {
      mocks.onMessage?.({
        event: "operations_changed",
        exercise_in_flight: [],
        emergency_wake_in_flight: false,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("ops-exercise")).toHaveTextContent("");
    });
    expect(screen.getByTestId("ops-emergency")).toHaveTextContent("false");
  });

  it("VACUITY GUARD: connected idle client makes zero 1s fetches over 5000ms of fake time", async () => {
    // Uses fake timers so we can advance 5000ms without waiting real-time.
    // FAILS on base (1Hz poll = 5 ticks in 5s) and PASSES on HEAD (no poll).
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const { getState, getOperations } = await import("../api/client");
      const getStateSpy = vi.mocked(getState);
      const getOperationsSpy = vi.mocked(getOperations);

      render(
        <LiveStateProvider>
          <SensorConsumer />
        </LiveStateProvider>,
      );

      // Wait for initial load to complete
      await waitFor(() => {
        expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
      });

      const baselineStateCalls = getStateSpy.mock.calls.length;
      const baselineOpsCalls = getOperationsSpy.mock.calls.length;

      // Advance 5000ms of fake time — if the 1Hz poll is still active,
      // that is 5 ticks and call counts grow. If gone (HEAD), they stay.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_000);
      });

      expect(getStateSpy.mock.calls.length).toBe(baselineStateCalls);
      expect(getOperationsSpy.mock.calls.length).toBe(baselineOpsCalls);
    } finally {
      vi.useRealTimers();
    }
  });

  it("30s reconciliation timer fires and refetches state+operations", async () => {
    // issue #184: replaces the removed 1Hz poll with a 30s reconciliation timer.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const { getState, getOperations } = await import("../api/client");
      const getStateSpy = vi.mocked(getState);
      const getOperationsSpy = vi.mocked(getOperations);

      render(
        <LiveStateProvider>
          <SensorConsumer />
        </LiveStateProvider>,
      );

      await waitFor(() => {
        expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
      });

      const preState = getStateSpy.mock.calls.length;
      const preOps = getOperationsSpy.mock.calls.length;

      // Advance past the 30s reconciliation tick and flush all pending async work.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(30_001);
      });

      // reconciliation fires refresh() which calls getState + getOperations
      expect(getStateSpy.mock.calls.length).toBeGreaterThan(preState);
      expect(getOperationsSpy.mock.calls.length).toBeGreaterThan(preOps);
    } finally {
      vi.useRealTimers();
    }
  });

  it("onConnect (reconnect) refetches state+operations", async () => {
    // issue #184: onConnect drives reconciliation refresh which hits both endpoints.
    const { getState, getOperations } = await import("../api/client");
    const getStateSpy = vi.mocked(getState);
    const getOperationsSpy = vi.mocked(getOperations);

    render(
      <LiveStateProvider>
        <SensorConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("sensor-s1")).toHaveTextContent("absent");
    });

    const preState = getStateSpy.mock.calls.length;
    const preOps = getOperationsSpy.mock.calls.length;

    act(() => {
      mocks.onConnect?.();
    });

    await waitFor(() => {
      expect(getStateSpy.mock.calls.length).toBeGreaterThan(preState);
    });
    expect(getOperationsSpy.mock.calls.length).toBeGreaterThan(preOps);
  });

  it("operations_changed event applies even when operations HTTP request is in flight", async () => {
    // issue #184: WS frame must win over an in-flight stale HTTP response.
    // Uses a deferred promise to simulate an HTTP request that hasn't resolved yet.
    let resolveOps: (v: { exercise_in_flight: string[]; emergency_wake_in_flight: boolean }) => void;
    const pendingOps = new Promise<{ exercise_in_flight: string[]; emergency_wake_in_flight: boolean }>(
      (r) => {
        resolveOps = r;
      },
    );
    const { getOperations } = await import("../api/client");
    vi.mocked(getOperations).mockReturnValue(pendingOps);

    function OperationsConsumer() {
      const { operations } = useLiveState();
      return (
        <div>
          <span data-testid="ops-ex">{operations?.exercise_in_flight.join(",") ?? "null"}</span>
          <span data-testid="ops-em">{String(operations?.emergency_wake_in_flight ?? "null")}</span>
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <OperationsConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("ops-ex")).toHaveTextContent("null");
    });

    // Deliver WS event while HTTP request is still pending — WS must win.
    act(() => {
      mocks.onMessage?.({
        event: "operations_changed",
        exercise_in_flight: ["studio"],
        emergency_wake_in_flight: true,
      });
    });

    await waitFor(() => {
      expect(screen.getByTestId("ops-ex")).toHaveTextContent("studio");
    });
    expect(screen.getByTestId("ops-em")).toHaveTextContent("true");

    // HTTP finally resolves with stale idle data — must NOT clobber the WS update.
    // The shared seq counter ensures WS commit (seq=N+1) dominates HTTP (seq=N).
    act(() => {
      resolveOps!({ exercise_in_flight: [], emergency_wake_in_flight: false });
    });

    await waitFor(() => {
      expect(screen.getByTestId("ops-ex")).toHaveTextContent("studio");
    });
    expect(screen.getByTestId("ops-em")).toHaveTextContent("true");
  });

});

// ── wear_sampling_source_gate live event ───────────────────────────────────

describe("LiveStateProvider wear_sampling_source_gate event", () => {
  it("wear_sampling_source_gate triggers a background wear refetch that updates the event's display row", async () => {
    const { getWear } = await import("../api/client");
    const getWearMock = vi.mocked(getWear);
    // Reset any leftover queued return values from prior tests (the shared
    // afterEach clears calls but not the mockResolvedValueOnce queue).
    getWearMock.mockReset();

    // Initial wear list: both displays on a matched gate.
    const initial: WearListResponse = {
      displays: [
        {
          display: "d1",
          display_name: "d1",
          panel_type: "qd-oled",
          total_on_hours: 1,
          sample_count: 1,
          advisory: false,
          hours_since_long_dwell: 0,
          source_gate: "matched",
        },
        {
          display: "d2",
          display_name: "d2",
          panel_type: "qd-oled",
          total_on_hours: 2,
          sample_count: 2,
          advisory: false,
          hours_since_long_dwell: 0,
          source_gate: "matched",
        },
      ],
    };
    // After the transition, only d1 (the event's display) flips to mismatched;
    // d2 is unchanged — the refetch updates the event's display row.
    const updated: WearListResponse = {
      displays: [
        {
          display: "d1",
          display_name: "d1",
          panel_type: "qd-oled",
          total_on_hours: 1,
          sample_count: 1,
          advisory: false,
          hours_since_long_dwell: 0,
          source_gate: "mismatched",
        },
        {
          display: "d2",
          display_name: "d2",
          panel_type: "qd-oled",
          total_on_hours: 2,
          sample_count: 2,
          advisory: false,
          hours_since_long_dwell: 0,
          source_gate: "matched",
        },
      ],
    };

    getWearMock.mockResolvedValue(initial);

    function WearListConsumer() {
      const { wear } = useLiveState();
      if (!wear) return <span>loading</span>;
      return (
        <div>
          {wear.displays.map((d) => (
            <span key={d.display} data-testid={`wear-gate-${d.display}`}>
              {d.display}:{d.source_gate ?? "none"}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <WearListConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("wear-gate-d1")).toHaveTextContent("d1:matched");
    });
    expect(screen.getByTestId("wear-gate-d2")).toHaveTextContent("d2:matched");

    const callsBefore = getWearMock.mock.calls.length;

    // Swap the wear list so the next reflects d1's transition to mismatched.
    getWearMock.mockResolvedValue(updated);

    // Drive a source-gate transition for d1 only.
    act(() => {
      mocks.onMessage?.({
        event: "wear_sampling_source_gate",
        display: "d1",
        state: "mismatched",
        observed: "HDMI3",
      });
    });

    // The event must trigger a background refetch of /api/wear.
    await waitFor(() => {
      expect(getWearMock.mock.calls.length).toBeGreaterThan(callsBefore);
    });

    // The refetch updates d1's row to mismatched; d2 stays matched.
    await waitFor(() => {
      expect(screen.getByTestId("wear-gate-d1")).toHaveTextContent("d1:mismatched");
    });
    expect(screen.getByTestId("wear-gate-d2")).toHaveTextContent("d2:matched");
  });

  it("an unknown WS event tag still leaves the wear list unchanged (additive union)", async () => {
    const { getWear } = await import("../api/client");
    const getWearMock = vi.mocked(getWear);
    getWearMock.mockReset();

    getWearMock.mockResolvedValue({
      displays: [
        {
          display: "d1",
          display_name: "d1",
          panel_type: "qd-oled",
          total_on_hours: 1,
          sample_count: 1,
          advisory: false,
          hours_since_long_dwell: 0,
          source_gate: "matched",
        },
      ],
    } as WearListResponse);

    function WearListConsumer() {
      const { wear } = useLiveState();
      if (!wear) return <span>loading</span>;
      return (
        <div>
          {wear.displays.map((d) => (
            <span key={d.display} data-testid={`wear-gate-${d.display}`}>
              {d.display}:{d.source_gate ?? "none"}
            </span>
          ))}
        </div>
      );
    }

    render(
      <LiveStateProvider>
        <WearListConsumer />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("wear-gate-d1")).toHaveTextContent("d1:matched");
    });

    const callsBefore = getWearMock.mock.calls.length;

    act(() => {
      mocks.onMessage?.({ event: "some_future_tag", display: "d1", whatever: 1 });
    });

    // Give any (incorrect) handler a chance to run, then assert no refetch.
    await new Promise((r) => setTimeout(r, 10));
    expect(getWearMock.mock.calls.length).toBe(callsBefore);
    expect(screen.getByTestId("wear-gate-d1")).toHaveTextContent("d1:matched");
  });
});

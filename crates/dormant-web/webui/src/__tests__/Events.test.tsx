/**
 * Events view test — verifies event-log rendering from the shared provider.
 *
 * Since the Events view now reads from useEventLog() (supplied by
 * LiveStateProvider), the test mocks useEventLog directly rather than
 * the underlying WebSocket hook.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import Events from "../app/views/Events";


const { mockUseEventLog } = vi.hoisted(() => {
  let eventsVal: Array<{ time: string; event: unknown }> = [];
  let connectedVal = true;
  let laggedVal = false;

  const impl = vi.fn(() => ({
    events: eventsVal,
    connected: connectedVal,
    lagged: laggedVal,
  }));

  return {
    mockUseEventLog: {
      impl,
      set events(v: Array<{ time: string; event: unknown }>) {
        eventsVal = v;
      },
      set connected(v: boolean) {
        connectedVal = v;
      },
      set lagged(v: boolean) {
        laggedVal = v;
      },
    },
  };
});

vi.mock("../app/hooks/useLiveState", () => ({
  useEventLog: mockUseEventLog.impl,
  useLiveState: vi.fn(() => ({
    loading: false,
    error: null,
    snapshot: null,
    config: null,
    connected: false,
    sensorConfigs: {},
    zoneConfigs: {},
    displayConfigs: {},
    displayRules: {},
    refresh: vi.fn(),
  })),
  LiveStateProvider: ({ children }: { children: React.ReactNode }) => children,
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mockUseEventLog.events = [];
  mockUseEventLog.connected = true;
  mockUseEventLog.lagged = false;
});

describe("Events", () => {
  it("renders empty state when no events have arrived", () => {
    render(<Events />);

    expect(
      screen.getByText("live · subscribed to daemon event stream"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Waiting for the first event…"),
    ).toBeInTheDocument();
    expect(screen.getByText("0 events")).toBeInTheDocument();
  });

  it("renders appending event rows after events arrive", () => {
    mockUseEventLog.events = [
      {
        time: "12:00:00",
        event: {
          event: "sensor_changed",
          sensor: "desk-mmwave",
          state: "present",
        },
      },
    ];

    render(<Events />);

    expect(screen.getByText("desk-mmwave → present")).toBeInTheDocument();
    expect(screen.getByText("sensor_changed")).toBeInTheDocument();
    expect(screen.getByText("1 events")).toBeInTheDocument();
  });

  it("renders multiple event types with correct badges", () => {
    mockUseEventLog.events = [
      {
        time: "12:00:00",
        event: {
          event: "zone_changed",
          zone: "office",
          present: true,
          cause: "radar",
        },
      },
      {
        time: "12:00:01",
        event: { event: "config_reloaded" },
      },
      {
        time: "12:00:02",
        event: {
          event: "wake_retry",
          display: "aoc-main",
          attempt: 2,
        },
      },
    ];

    render(<Events />);

    expect(
      screen.getByText("zone 'office' → occupied (cause: radar)"),
    ).toBeInTheDocument();
    expect(screen.getByText("config reloaded")).toBeInTheDocument();
    expect(
      screen.getByText("aoc-main: wake retry attempt 2"),
    ).toBeInTheDocument();

    expect(screen.getByText("zone_changed")).toBeInTheDocument();
    expect(screen.getByText("config_reloaded")).toBeInTheDocument();
    expect(screen.getByText("wake_retry")).toBeInTheDocument();
  });

  it("shows lagged banner when lagged is true", () => {
    mockUseEventLog.lagged = true;

    render(<Events />);

    expect(
      screen.getByText("stream lagged — catching up"),
    ).toBeInTheDocument();
    expect(screen.getByText("0 events")).toBeInTheDocument();
  });

  it("shows event count", () => {
    mockUseEventLog.events = [
      {
        time: "12:00:00",
        event: {
          event: "sensor_changed",
          sensor: "a",
          state: "present",
        },
      },
      {
        time: "12:00:01",
        event: {
          event: "zone_changed",
          zone: "o",
          present: true,
          cause: "x",
        },
      },
    ];

    render(<Events />);

    expect(screen.getByText("2 events")).toBeInTheDocument();
  });

  it("renders config_reload_rejected with detail", () => {
    mockUseEventLog.events = [
      {
        time: "12:00:00",
        event: {
          event: "config_reload_rejected",
          detail: "invalid sensor config: unknown type 'foo'",
        },
      },
    ];

    render(<Events />);

    expect(
      screen.getByText(
        "config reload rejected: invalid sensor config: unknown type 'foo'",
      ),
    ).toBeInTheDocument();
    expect(screen.getByText("config_reload_rejected")).toBeInTheDocument();
    expect(screen.getByText("1 events")).toBeInTheDocument();
  });

  it("handles unknown event tag gracefully", () => {
    mockUseEventLog.events = [
      {
        time: "12:00:00",
        event: {
          event: "future_event_v2",
          payload: "test",
        },
      },
    ];

    // Must not throw — the default arm in messageForEvent produces
    // JSON.stringify output and the default badge arm labels by
    // event name.
    expect(() => {
      render(<Events />);
    }).not.toThrow();

    // The event count still renders.
    expect(screen.getByText("1 events")).toBeInTheDocument();
    // The badge label is the event name.
    expect(screen.getByText("future_event_v2")).toBeInTheDocument();
    // The message is the JSON representation.
    expect(
      screen.getByText('{"event":"future_event_v2","payload":"test"}'),
    ).toBeInTheDocument();
  });

  it("hides switching filter group when no ownership events exist", () => {
    mockUseEventLog.events = [
      { time: "12:00:00", event: { event: "sensor_changed", sensor: "a", state: "present" } },
    ];
    render(<Events />);
    // The "switching" filter chip should NOT be visible.
    expect(screen.queryByText(/switching/i)).not.toBeInTheDocument();
  });

  it("shows switching filter group after ownership event arrives", () => {
    mockUseEventLog.events = [
      { time: "12:00:00", event: { event: "ownership" as const, display: "d", owned: true, cause: "pull" } },
    ];
    render(<Events />);
    // The filter chip with label "switching" should be visible.
    // getByText works because button text is just the label.
    expect(screen.queryByText("switching")).not.toBeNull();
  });

  it("renders history separator sentinel as a divider", () => {
    mockUseEventLog.events = [
      { time: "12:00:01", event: { event: "_history_separator" } },
      { time: "12:00:00", event: { event: "sensor_changed", sensor: "a", state: "present" } },
    ];
    render(<Events />);
    expect(screen.getByText(/— history —/)).toBeInTheDocument();
  });

  it("history separator survives active tag filters", () => {
    mockUseEventLog.events = [
      { time: "12:00:01", event: { event: "_history_separator" } },
      { time: "12:00:00", event: { event: "zone_changed", zone: "z", present: true, cause: "s" } },
    ];
    // Simulate active filter by updating hash params
    window.location.hash = "#/events?tag=zone_changed";
    render(<Events />);
    // Separator should still be visible even though it's not in any filter group.
    expect(screen.getByText(/— history —/)).toBeInTheDocument();
    // But the zone_changed event should also be visible (matching filter).
    expect(screen.getByText(/zone 'z'/)).toBeInTheDocument();
  });

  it("clicking a filter chip toggles the hash query parameter", () => {
    mockUseEventLog.events = [
      { time: "12:00:00", event: { event: "zone_changed", zone: "z", present: true, cause: "s" } },
    ];
    window.location.hash = "#/events";
    render(<Events />);

    // Click the "presence" filter group (covers zone_changed).
    const presenceChip = screen.getByText("presence");
    fireEvent.click(presenceChip);

    // Hash should now include ?tag=zone_changed,sensor_changed
    // (the presence group covers both tags).
    expect(window.location.hash).toContain("?tag=");
    expect(window.location.hash).toContain("zone_changed");
    expect(window.location.hash).toContain("sensor_changed");
  });
});

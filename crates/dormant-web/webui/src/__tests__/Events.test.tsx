/**
 * Events view test — verifies event-log rendering from the shared provider.
 *
 * Since the Events view now reads from useEventLog() (supplied by
 * LiveStateProvider), the test mocks useEventLog directly rather than
 * the underlying WebSocket hook.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
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
      screen.getByText("Waiting for events from the daemon…"),
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
    expect(screen.getByText("sensor")).toBeInTheDocument();
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

    expect(screen.getByText("zone")).toBeInTheDocument();
    expect(screen.getByText("config")).toBeInTheDocument();
    expect(screen.getByText("retry")).toBeInTheDocument();
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
});

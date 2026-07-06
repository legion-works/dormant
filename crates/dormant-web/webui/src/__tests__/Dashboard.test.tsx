/**
 * Dashboard component test — renders with recorded fixture data.
 *
 * Fixture data is hoisted alongside the mock so it is available
 * before the hoisted vi.mock factory runs.  This avoids the
 * "Cannot access before initialization" error.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup } from "@testing-library/react";
import Dashboard from "../app/views/Dashboard";
import { LiveStateProvider } from "../app/state";
import { EventLogContext } from "../app/hooks/useLiveState";
import type { StampedEvent } from "../app/hooks/useLiveState";


const { SAMPLE_STATE, SAMPLE_CONFIG } = vi.hoisted(() => ({
  SAMPLE_STATE: {
    sensors: [
      { id: "desk-mmwave", state: "present" as const, last_seen_secs_ago: 3 },
      { id: "room-pir", state: "absent" as const, last_seen_secs_ago: 45 },
      { id: "balcony-mqtt", state: "unavailable" as const, last_seen_secs_ago: 120 },
    ],
    zones: [
      { id: "office", present: true },
      { id: "hallway", present: false },
    ],
    displays: [
      [
        "aoc-main",
        { phase: "active", inhibited: false, paused: false, cmd_gen: 42, controllers: [{ name: "ddcci", role: "primary" as const, healthy: true }] },
      ],
      [
        "samsung-tv",
        { phase: "blanked", inhibited: false, paused: true, cmd_gen: 15, controllers: [{ name: "samsung-tizen", role: "primary" as const, healthy: true }] },
      ],
    ],
    pending_reload: null,
  },
  SAMPLE_CONFIG: {
    path: "/tmp/config.toml",
    config_version: 1,
    source: "last_applied",
    raw_toml: "",
    inventory: {
      config_version: 1,
      daemon: {},
      sensors: {
        "desk-mmwave": { type: "usb-ld2410" as const, port: "/dev/ttyUSB0" },
        "room-pir": { type: "mqtt" as const, broker_url: "", topic: "" },
        "balcony-mqtt": { type: "ha" as const, url: "", entity: "" },
      },
      zones: {
        office: { mode: "any", members: ["desk-mmwave", "room-pir"], weights: {}, unavailable_policy: "present" as const },
        hallway: { mode: "all", members: ["room-pir"], weights: {}, unavailable_policy: "absent" as const },
      },
      displays: {
        "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off" as const },
        "samsung-tv": { controllers: ["samsung-tizen"], blank_mode: "screen_off_audio_on" as const },
      },
      rules: {
        "office-rule": { zone: "office", displays: ["aoc-main"], wake_retries: 3 },
        "tv-rule": { zone: "hallway", displays: ["samsung-tv"], wake_retries: 5 },
      },
    },
    validation: { ok: true, warnings: [], errors: [] },
    display_rules: {
      "aoc-main": { rule: "office-rule", zone: "office" },
      "samsung-tv": { rule: "tv-rule", zone: "hallway" },
    },
  },
}));

// Mock the API client and WS hook so LiveStateProvider can initialise.
vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  postBlank: vi.fn().mockResolvedValue(undefined),
  postWake: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("../api/ws", () => ({
  useEvents: vi.fn(() => ({ connected: false, close: vi.fn() })),
}));

afterEach(() => cleanup());

describe("Dashboard", () => {
  it("renders the four stat cards after loading", async () => {
    render(<LiveStateProvider><Dashboard /></LiveStateProvider>);

    await waitFor(() => {
      const labels = screen.getAllByText("Displays");
      expect(labels.length).toBeGreaterThanOrEqual(2);
    });

    expect(screen.getByText("2")).toBeInTheDocument();
    expect(screen.getByText("2/3")).toBeInTheDocument();
    expect(screen.getByText("1/2")).toBeInTheDocument();
    expect(screen.getByText("Active")).toBeInTheDocument();
  });

  it("renders sensor rows with correct state labels", async () => {
    render(<LiveStateProvider><Dashboard /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("desk-mmwave")).toBeInTheDocument();
    });

    // "present"/"absent"/"unavailable" appear in sensor rows AND zone rows
    expect(screen.getAllByText("present").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("absent").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("unavailable").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("LD2410 radar")).toBeInTheDocument();
    expect(screen.getByText("MQTT")).toBeInTheDocument();
  });

  it("renders zone rows with mode and members", async () => {
    render(<LiveStateProvider><Dashboard /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("office")).toBeInTheDocument();
    });

    expect(screen.getByText("ANY")).toBeInTheDocument();
  });

  it("renders display rows with blank/wake buttons and config metadata", async () => {
    render(<LiveStateProvider><Dashboard /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getAllByText("blank").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("wake").length).toBeGreaterThanOrEqual(1);

    // MUST 1: blank_mode and controller chain from config
    expect(screen.getByText("power_off")).toBeInTheDocument();
    expect(screen.getByText("ddcci")).toBeInTheDocument();
  });

  it("shows section headers", async () => {
    render(<LiveStateProvider><Dashboard /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("Signal flow")).toBeInTheDocument();
    });

    expect(screen.getByText(/sensors → zones → displays/)).toBeInTheDocument();
    expect(screen.getByText("Recent activity")).toBeInTheDocument();
    expect(screen.getByText("view all →")).toBeInTheDocument();
  });

  it("shows empty state in recent activity when event log is empty", async () => {
    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={{ events: [], connected: true, lagged: false }}>
          <Dashboard />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("No recent events from the daemon.")).toBeInTheDocument();
    });
  });

  it("renders recent activity from the event log", async () => {
    const mockEvents: StampedEvent[] = [
      {
        time: "14:23:01",
        event: {
          event: "sensor_changed",
          sensor: "desk-mmwave",
          state: "present",
        },
      },
    ];

    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={{ events: mockEvents, connected: true, lagged: false }}>
          <Dashboard />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("desk-mmwave → present")).toBeInTheDocument();
    });
    expect(screen.getByText("sensor")).toBeInTheDocument();
  });
});

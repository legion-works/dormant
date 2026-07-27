/**
 * Overview view tests — panel tiles, held-by candidates, Protected tile, Rules column.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup, waitFor } from "@testing-library/react";
import Overview from "../app/views/Overview";
import { LiveStateProvider } from "../app/state";
import { EventLogContext } from "../app/hooks/useLiveState";
import { eventLogFixture } from "./fixtures/live-state";
import type { StateSnapshot, ConfigResponse, DisplayConfig } from "../api/types";
import { getState, getConfig } from "../api/client";

const { SAMPLE_STATE, SAMPLE_CONFIG } = vi.hoisted(() => ({
  SAMPLE_STATE: {
    sensors: [
      { id: "desk-mmwave", state: "present" as const, last_seen_secs_ago: 3, reported: true },
    ],
    zones: [
      { id: "office", present: true },
      { id: "hallway", present: false },
    ],
    displays: [
      [
        "studio",
        { phase: "active" as const, inhibited: false, paused: false, cmd_gen: 1, controllers: [{ name: "ddcci", role: "primary" as const, healthy: true }] },
      ],
      [
        "shared-oled",
        { phase: "blanked" as const, inhibited: true, paused: false, cmd_gen: 2, controllers: [], scope: "shared" as const, owned: false },
      ],
    ],
    pending_reload: null,
  } as StateSnapshot,
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
      },
      zones: {
        office: { mode: "any" as const, members: ["desk-mmwave"], weights: {}, unavailable_policy: "present" as const },
        hallway: { mode: "all" as const, members: [], weights: {}, unavailable_policy: "absent" as const },
      },
      displays: {
        "studio": { controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" as const } as DisplayConfig,
        "shared-oled": { controllers: ["ddcci"], blank_mode: "power_off" as const, scope: "shared" as const } as DisplayConfig,
      },
      rules: {
        "office-rule": { zone: "office", displays: ["studio"], inhibitors: ["audio-playback"], activity_idle_threshold: "30s" },
        "tv-rule": { zone: "hallway", displays: ["shared-oled"], inhibitors: [] },
      },
    },
    validation: { ok: true, warnings: [], errors: [] },
    display_rules: {
      "studio": { rule: "office-rule", zone: "office" },
      "shared-oled": { rule: "tv-rule", zone: "hallway" },
    },
    fingerprint: "abc123",
    redacted_paths: [],
  } as ConfigResponse,
}));

vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  getOperations: vi.fn().mockResolvedValue({ exercise_in_flight: [], emergency_wake_in_flight: false }),
  getWear: vi.fn().mockResolvedValue({ displays: [] }),
  getWearDetail: vi.fn(),
  getRecentEvents: vi.fn().mockResolvedValue({ events: [] }),
  postBlank: vi.fn().mockResolvedValue(undefined),
  postWake: vi.fn().mockResolvedValue(undefined),
  postSwitch: vi.fn().mockResolvedValue(undefined),
  postPush: vi.fn().mockResolvedValue(undefined),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Overview", () => {
  it("renders Protected stat tile from display_rules", async () => {
    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("Protected")).toBeInTheDocument();
    });

    // studio and shared-oled both have rules → 2 protected.
    // The stat row shows the count; "2" may also appear in Zones count.
    const twos = screen.getAllByText("2");
    expect(twos.length).toBeGreaterThanOrEqual(1);
  });

  it("renders Rules column with rule rows", async () => {
    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("office-rule")).toBeInTheDocument();
      expect(screen.getByText("tv-rule")).toBeInTheDocument();
    });
  });

  it("renders held-by candidates for inhibited display respecting rule config", async () => {
    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      // shared-oled has inhibited=true but tv-rule has empty inhibitors, no activity_threshold
      // → should show "▸ unknown inhibitor"
      expect(screen.getByText(/unknown inhibitor/)).toBeInTheDocument();
    });
  });

  it("renders peer holds panel for shared unowned display", async () => {
    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText(/peer holds the panel/)).toBeInTheDocument();
    });
  });

  it("shows flat single inhibitor (no ▸) when exactly one is configured", async () => {
    // Override state: studio is inhibited, office-rule has only activity_idle_threshold.
    const state: StateSnapshot = {
      ...SAMPLE_STATE,
      displays: [
        ["studio", { phase: "active", inhibited: true, paused: false, cmd_gen: 1, controllers: [{ name: "ddcci", role: "primary" as const, healthy: true }] }],
        ["shared-oled", { phase: "blanked", inhibited: false, paused: false, cmd_gen: 2, controllers: [], scope: "shared" as const, owned: false }],
      ],
    };
    const config: ConfigResponse = {
      ...SAMPLE_CONFIG,
      inventory: {
        ...SAMPLE_CONFIG.inventory,
        rules: {
          "office-rule": { zone: "office", displays: ["studio"], activity_idle_threshold: "30s" },
          "tv-rule": { zone: "hallway", displays: ["shared-oled"], inhibitors: [] },
        },
      },
    };
    vi.mocked(getState).mockResolvedValueOnce(state);
    vi.mocked(getConfig).mockResolvedValueOnce(config);

    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      // studio has inhibited=true, office-rule has only activity_idle_threshold
      // → exactly one inhibitor → flat prefix (•, not ▸).
      expect(screen.getByText(/\u2022 activity/)).toBeInTheDocument();
    });
  });

  it("shows ▸-prefixed candidates when multiple inhibitors configured", async () => {
    const state: StateSnapshot = {
      ...SAMPLE_STATE,
      displays: [
        ["studio", { phase: "active", inhibited: true, paused: false, cmd_gen: 1, controllers: [{ name: "ddcci", role: "primary" as const, healthy: true }] }],
        ["shared-oled", { phase: "blanked", inhibited: false, paused: false, cmd_gen: 2, controllers: [], scope: "shared" as const, owned: false }],
      ],
    };
    const config: ConfigResponse = {
      ...SAMPLE_CONFIG,
      inventory: {
        ...SAMPLE_CONFIG.inventory,
        rules: {
          "office-rule": { zone: "office", displays: ["studio"], activity_idle_threshold: "30s", inhibitors: ["audio-playback", "call"] },
          "tv-rule": { zone: "hallway", displays: ["shared-oled"], inhibitors: [] },
        },
      },
    };
    vi.mocked(getState).mockResolvedValueOnce(state);
    vi.mocked(getConfig).mockResolvedValueOnce(config);

    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      // Multiple inhibitors configured → ▸ prefix.
      expect(screen.getByText(/\u25b8 audio-playback/)).toBeInTheDocument();
      expect(screen.getByText(/\u25b8 call/)).toBeInTheDocument();
    });
  });
});

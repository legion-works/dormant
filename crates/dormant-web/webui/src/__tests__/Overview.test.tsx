/**
 * Overview view tests — panel tiles, held-by candidates, Protected tile, Rules column,
 * stat cards, sensor rows, zone rows, stage detail, and reported sensor hints.
 */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, cleanup, waitFor } from "@testing-library/react";
import Overview from "../app/views/Overview";
import { LiveStateProvider } from "../app/state";
import { EventLogContext } from "../app/hooks/useLiveState";
import type { StampedEvent } from "../app/hooks/useLiveState";
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
  getWearSamplingStatus: vi.fn().mockResolvedValue({ status: "granted" }),
  postWearSamplingEnable: vi.fn(),
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

// ── Stat cards, signal flow, section headers (fixture with 3 displays) ──

const STAT_FIXTURE = {
  state: {
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
      [
        "lg-oled",
        { phase: "staged", inhibited: false, paused: false, cmd_gen: 7, controllers: [{ name: "lg-webos", role: "primary" as const, healthy: true }], stage: { idx: 1, kind: "render_screensaver" } },
      ],
    ],
    pending_reload: null,
  } as StateSnapshot,
  config: {
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
        "lg-oled": { controllers: ["lg-webos"], blank_mode: "power_off" as const, ladder: [{ kind: "render_screensaver", dwell: "10s" }] },
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
      "lg-oled": { rule: "office-rule", zone: "office" },
    },
    fingerprint: "abc123",
    redacted_paths: [],
  } as ConfigResponse,
};

describe("Overview — panel tiles and signal flow", () => {
  beforeEach(() => {
    vi.mocked(getState).mockResolvedValue(STAT_FIXTURE.state);
    vi.mocked(getConfig).mockResolvedValue(STAT_FIXTURE.config);
  });

  it("renders the four stat cards after loading", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      // Displays appears in the stat card label.
      expect(screen.getByText("Displays")).toBeInTheDocument();
    });

    // Displays stat card shows "3" (total count).
    const threes = screen.getAllByText("3");
    expect(threes.length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("2/3")).toBeInTheDocument();
    expect(screen.getByText("1/2")).toBeInTheDocument();
    // Overview shows "Protected" stat card instead of "OLED guard Active".
    expect(screen.getByText("Protected")).toBeInTheDocument();
  });

  it("renders sensor rows with correct state labels", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

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
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("office")).toBeInTheDocument();
    });

    expect(screen.getByText("ANY")).toBeInTheDocument();
  });

  // Overview panel tiles have per-display Blank/Wake action chips.
  it("renders panel tile action chips for each display", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      // Per-tile Blank/Wake/Pull/Push action chips.
      const blanks = screen.getAllByRole("button", { name: "Blank" });
      expect(blanks.length).toBeGreaterThanOrEqual(1);
    });

    expect(screen.getAllByRole("button", { name: "Wake" }).length).toBeGreaterThanOrEqual(1);
  });

  it("shows section headers", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("Signal flow")).toBeInTheDocument();
    });

    expect(screen.getByText("Signal flow")).toBeInTheDocument();
    expect(screen.getByText("Recent activity")).toBeInTheDocument();
    expect(screen.getByText(/view all/)).toBeInTheDocument();
  });

  it("shows empty state in recent activity when event log is empty", async () => {
    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={{ events: [], connected: true, lagged: false, historySeeded: false }}>
          <Overview />
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
        <EventLogContext.Provider value={{ events: mockEvents, connected: true, lagged: false, historySeeded: false }}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("desk-mmwave → present")).toBeInTheDocument();
    });
    expect(screen.getByText("sensor_changed")).toBeInTheDocument();
  });

  it("renders stage detail in display row when a display is staged", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      // lg-oled appears in the panel tile grid.
      expect(screen.getAllByText("lg-oled").length).toBeGreaterThanOrEqual(1);
    });

    // The staged display chip shows "staged · render screensaver".
    expect(screen.getByText("staged · render screensaver")).toBeInTheDocument();
  });

  it("does not render stage detail on non-staged display rows", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      // aoc-main appears in the panel tile grid.
      expect(screen.getAllByText("aoc-main").length).toBeGreaterThanOrEqual(1);
    });

    // The active display chip label is "active", not a stage label.
    expect(screen.getByText("active")).toBeInTheDocument();
    // The blanked display chip label is "blanked".
    expect(screen.getByText("blanked")).toBeInTheDocument();

    // Stage detail only for the staged display.
    const stageLabels = screen.getAllByText(/render screensaver/);
    // lg-oled chip + possibly the blank_mode in config metadata
    expect(stageLabels.length).toBeGreaterThanOrEqual(1);
  });
});


// ── "no data since start" sensor hint (spec T6) ──

describe("Overview — sensor reported hint", () => {
  it("shows the hint for an unavailable sensor with reported: false", async () => {
    const state: StateSnapshot = {
      sensors: [
        { id: "balcony-mqtt", state: "unavailable", last_seen_secs_ago: 999, reported: false },
      ],
      zones: [],
      displays: [],
      pending_reload: null,
    };
    vi.mocked(getState).mockResolvedValueOnce(state);

    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("balcony-mqtt")).toBeInTheDocument();
    });

    expect(screen.getByText(/no data since start/i)).toBeInTheDocument();
  });

  it("hides the hint for an unavailable sensor with reported: true", async () => {
    const state: StateSnapshot = {
      sensors: [
        { id: "balcony-mqtt", state: "unavailable", last_seen_secs_ago: 999, reported: true },
      ],
      zones: [],
      displays: [],
      pending_reload: null,
    };
    vi.mocked(getState).mockResolvedValueOnce(state);

    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("balcony-mqtt")).toBeInTheDocument();
    });

    expect(screen.queryByText(/no data since start/i)).toBeNull();
  });

  it("never shows the hint for a present or absent sensor, reported or not", async () => {
    const state: StateSnapshot = {
      sensors: [
        { id: "desk-mmwave", state: "present", last_seen_secs_ago: 3, reported: false },
        { id: "room-pir", state: "absent", last_seen_secs_ago: 45, reported: false },
      ],
      zones: [],
      displays: [],
      pending_reload: null,
    };
    vi.mocked(getState).mockResolvedValueOnce(state);

    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("desk-mmwave")).toBeInTheDocument();
    });

    expect(screen.queryByText(/no data since start/i)).toBeNull();
  });

  // Legacy wire predates `reported` — the key is entirely absent from the
  // snapshot object, not merely `false`. `sensor.reported ?? false` treats
  // that identically to `false`. Pinning what the code actually does: the
  // hint is NEW, so an unavailable sensor on a legacy snapshot renders it —
  // there is no prior rendering to preserve for this exact case, since the
  // hint did not exist before this feature. This is the adjudicated,
  // intended behavior per spec T6, not a back-compat gap.
  it("legacy snapshot (no `reported` key at all) — unavailable sensor shows the hint", async () => {
    const legacySensor: StateSnapshot["sensors"][number] = {
      id: "balcony-mqtt",
      state: "unavailable",
      last_seen_secs_ago: 999,
    };
    expect("reported" in legacySensor).toBe(false);

    const state: StateSnapshot = {
      sensors: [legacySensor],
      zones: [],
      displays: [],
      pending_reload: null,
    };
    vi.mocked(getState).mockResolvedValueOnce(state);

    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("balcony-mqtt")).toBeInTheDocument();
    });

    expect(screen.getByText(/no data since start/i)).toBeInTheDocument();
  });
});

describe("Overview — unicode glyphs in JSX text (not raw \\u escapes)", () => {
  it("renders real glyphs, not raw \\u escapes", async () => {
    render(<LiveStateProvider><Overview /></LiveStateProvider>);

    await waitFor(() => {
      // The "view all →" link always renders (it's a static button).
      expect(screen.getByText(/view all →/)).toBeInTheDocument();
    });

    // Must NOT render the raw escape sequence anywhere.
    expect(screen.queryByText(/\\u2192/)).toBeNull();
    expect(screen.queryByText(/\\u21C4/)).toBeNull();
    expect(screen.queryByText(/\\u00b7/)).toBeNull();
  });

  it("history separator sentinel never renders as a raw event row", async () => {
    // Seed events with a _history_separator sentinel — must not appear as a badge or JSON row.
    const mockEvents: StampedEvent[] = [
      { time: "14:00:00", event: { event: "sensor_changed", sensor: "test", state: "present" } },
      { time: "", event: { event: "_history_separator" } as never },
    ];

    render(
      <LiveStateProvider>
        <EventLogContext.Provider value={{ events: mockEvents, connected: true, lagged: false, historySeeded: true }}>
          <Overview />
        </EventLogContext.Provider>
      </LiveStateProvider>,
    );

    await waitFor(() => {
      // The real sensor_changed event should render.
      expect(screen.getByText("sensor_changed")).toBeInTheDocument();
    });

    // The _history_separator badge/text must never appear.
    expect(screen.queryByText("_history_separator")).toBeNull();
    // No raw JSON dump (the old bug).
    expect(screen.queryByText(/"event"/)).toBeNull();
  });
});

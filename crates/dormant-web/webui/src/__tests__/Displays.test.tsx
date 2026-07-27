/**
 * Displays component test — W3-1 row-based list layout with
 * shared/private grouping, held-by column, and full chip set.
 */
import { useState } from "react";
import { describe, it, expect, vi, afterEach } from "vitest";
import { act, render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import Displays from "../app/views/Displays";
import { LiveStateProvider } from "../app/state";
import { LiveStateContext } from "../app/hooks/useLiveState";
import { liveStateFixture } from "./fixtures/live-state";
import type { DisplayConfig, DisplaySnapshot } from "../api/types";


const { SAMPLE_STATE, SAMPLE_CONFIG, mocks } = vi.hoisted(() => {
  const postBlank = vi.fn().mockResolvedValue(undefined);
  const postWake = vi.fn().mockResolvedValue(undefined);
  const postPause = vi.fn().mockResolvedValue(undefined);
  const postResume = vi.fn().mockResolvedValue(undefined);
  const postSwitch = vi.fn().mockResolvedValue({ verdict: "accepted", deadline_ms: 123 });
  const postPush = vi.fn().mockResolvedValue(undefined);
  return {
    mocks: { postBlank, postWake, postPause, postResume, postSwitch, postPush },
    SAMPLE_STATE: {
      sensors: [
        { id: "desk-mmwave", state: "present" as const, last_seen_secs_ago: 3 },
      ],
      zones: [
        { id: "office", present: true },
      ],
      displays: [
        [
          "aoc-main",
          {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 42,
            controllers: [
              { name: "ddcci", role: "primary" as const, healthy: true },
              { name: "kwin-dpms", role: "fallback" as const, healthy: false, detail: "DBus timeout" },
            ],
          },
        ],
        [
          "samsung-tv",
          {
            phase: "blanked",
            inhibited: false,
            paused: true,
            cmd_gen: 15,
            controllers: [
              { name: "samsung-tizen", role: "primary" as const, healthy: true },
            ],
          },
        ],
        [
          "lg-oled",
          {
            phase: "staged",
            inhibited: false,
            paused: false,
            cmd_gen: 7,
            controllers: [{ name: "lg-webos", role: "primary" as const, healthy: true }],
            stage: { idx: 0, kind: "render_black" },
          },
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
        },
        zones: {
          office: { mode: "any", members: ["desk-mmwave"], weights: {}, unavailable_policy: "present" as const },
        },
        displays: {
          "aoc-main": { controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" as const },
          "samsung-tv": { controllers: ["samsung-tizen"], blank_mode: "screen_off_audio_on" as const },
          "lg-oled": { controllers: ["lg-webos"], blank_mode: "power_off" as const, ladder: [{ kind: "render_black", dwell: "5s" }] },
        },
        rules: {
          "office-rule": { zone: "office", displays: ["aoc-main"], wake_retries: 3 },
          "tv-rule": { zone: "office", displays: ["samsung-tv"], wake_retries: 5 },
        },
      },
      validation: { ok: true, warnings: [], errors: [] },
      display_rules: {
        "aoc-main": { rule: "office-rule", zone: "office" },
        "samsung-tv": { rule: "tv-rule", zone: "office" },
        "lg-oled": { rule: "office-rule", zone: "office" },
      },
    },
  };
});

vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  postBlank: mocks.postBlank,
  postWake: mocks.postWake,
  postPause: mocks.postPause,
  postResume: mocks.postResume,
  postSwitch: mocks.postSwitch,
  postPush: mocks.postPush,
  getWear: vi.fn().mockResolvedValue({ displays: [] }),
  getWearDetail: vi.fn().mockRejectedValue(new Error("unexpected wear detail request")),
  getOperations: vi.fn().mockResolvedValue({
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  }),
}));

vi.mock("../api/ws", () => ({
  useEvents: vi.fn(() => ({ connected: false, close: vi.fn() })),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function renderDisplayCard(id: string, display: DisplaySnapshot, overrides: {
  scope?: "shared";
  config?: Partial<DisplayConfig>;
} = {}) {
  const dc: DisplayConfig = {
    controllers: [],
    blank_mode: "power_off",
    scope: overrides.scope,
    ...overrides.config,
  } as DisplayConfig;
  const state = liveStateFixture({
    snapshot: {
      sensors: [],
      zones: [],
      displays: [[id, display]],
      pending_reload: null,
      kvm: { keymap: {}, switch_capable_displays: [id], activity_following: false, push_capable_displays: [] },
    },
    displayConfigs: { [id]: dc },
    displayRules: { [id]: { rule: "office-rule", zone: "office" } },
  });
  render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
}

function sharedDisplay(overrides: Partial<DisplaySnapshot> = {}): DisplaySnapshot {
  return {
    phase: "active",
    inhibited: false,
    paused: false,
    cmd_gen: 1,
    scope: "shared",
    owned: true,
    observed_input_code: 96,
    panel_state: { power: "standby" },
    controllers: [],
    ...overrides,
  };
}

describe("Displays", () => {
  it("renders display IDs and phases in row layout", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("samsung-tv")).toBeInTheDocument();
    // Phase chips render their labels
    expect(screen.getByText("active")).toBeInTheDocument();
    expect(screen.getByText("blanked")).toBeInTheDocument();
  });

  it("renders the full four-chip set (paused, inhibited, blank_failed, wear_advisory)", async () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["test-disp", {
          phase: "active",
          inhibited: true,
          paused: true,
          cmd_gen: 1,
          controllers: [],
          last_blank_failed: true,
        }]],
        pending_reload: null,
      },
      displayConfigs: {"test-disp": { controllers: [], blank_mode: "power_off" } as DisplayConfig},
      displayRules: {"test-disp": { rule: "test-rule", zone: "test" }},
      wear: { displays: [{ display: "test", display_name: "test-disp", panel_type: "unknown", total_on_hours: 1, sample_count: 1, advisory: true, hours_since_long_dwell: 1 }] },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);

    expect(screen.getByText("paused")).toBeInTheDocument();
    expect(screen.getByText("inhibited")).toBeInTheDocument();
    expect(screen.getByText("blank failed")).toBeInTheDocument();
    expect(screen.getByText("wear advisory")).toBeInTheDocument();
  });

  it("renders controller health chips", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("ddcci")).toBeInTheDocument();
    expect(screen.getByText("kwin-dpms")).toBeInTheDocument();
    expect(screen.getAllByText("primary").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("fallback").length).toBeGreaterThanOrEqual(1);
  });

  it("renders held-by column with rule and zone", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // Held-by column shows "office-rule · office"
    expect(screen.getAllByText("office-rule · office").length).toBeGreaterThanOrEqual(1);
    // tv-rule also exists
    expect(screen.getByText("tv-rule · office")).toBeInTheDocument();
  });

  it("calls postBlank guarded by confirmation, and postPause guarded by confirmation, with correct ids", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });
    expect(screen.getByText("samsung-tv")).toBeInTheDocument();

    // Force blank on first display (aoc-main). When dialog opens, every
    // row's action column hides — only the dialog button remains.
    fireEvent.click(screen.getAllByText("Force blank")[0]);
    expect(screen.getByRole("alertdialog", { name: "Force blank aoc-main?" })).toBeInTheDocument();
    // All row action buttons are hidden while the dialog is open.
    expect(screen.queryAllByRole("button", { name: /Force wake/ })).toHaveLength(0);
    fireEvent.click(screen.getByRole("button", { name: "Force blank" }));
    // Issue #124 — the web tray "Force blank" path is always Hard.
    await waitFor(() => expect(mocks.postBlank).toHaveBeenCalledWith("aoc-main", "hard"));

    // Pause rule on first display (aoc-main → rule "office-rule")
    fireEvent.click(screen.getAllByText("Pause rule")[0]);
    expect(screen.getByRole("alertdialog", { name: "Pause office-rule?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Pause rule" }));
    await waitFor(() => expect(mocks.postPause).toHaveBeenCalledWith({ rule: "office-rule" }));
  });

  // P1-F: Force wake and Resume are non-destructive (wake just lights the
  // panel) — the proto's friction model leaves them un-gated.
  it("calls postWake/postResume immediately with correct ids, no confirm dialog", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });
    expect(screen.getByText("samsung-tv")).toBeInTheDocument();

    fireEvent.click(screen.getAllByText("Force wake")[0]);
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    await waitFor(() => expect(mocks.postWake).toHaveBeenCalledWith("aoc-main"));

    // Resume rule on the paused display (samsung-tv → rule "tv-rule")
    const resumeBtns = screen.getAllByText("Resume rule");
    expect(resumeBtns.length).toBeGreaterThanOrEqual(1);
    fireEvent.click(resumeBtns[0]);
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    await waitFor(() => expect(mocks.postResume).toHaveBeenCalledWith({ rule: "tv-rule" }));
  });

  it("does not post when the force blank confirmation is cancelled", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    const flush = () => act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    fireEvent.click(screen.getAllByText("Force blank")[0]);
    expect(screen.getByRole("alertdialog", { name: "Force blank aoc-main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flush();
    expect(mocks.postBlank).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  });

  it("has Force wake and Pause/Resume buttons for each display", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("samsung-tv")).toBeInTheDocument();
    });

    expect(screen.getAllByText("Force wake").length).toBeGreaterThanOrEqual(1);
    // samsung-tv is paused → "Resume rule"
    expect(screen.getAllByText("Resume rule").length).toBeGreaterThanOrEqual(1);
    // aoc-main is not paused → "Pause rule"
    expect(screen.getAllByText("Pause rule").length).toBeGreaterThanOrEqual(1);
  });

  it("shared display shows peer-holds-panel in held-by when not owned", () => {
    renderDisplayCard("shared-tv", sharedDisplay({ owned: false }), { scope: "shared" });

    // Phase dot shows for active
    expect(screen.getByText("shared-tv")).toBeInTheDocument();
    // Held-by column: shared + not owned → "peer holds panel"
    expect(screen.getByText("peer holds panel")).toBeInTheDocument();
  });

  it("shared display shows rule · zone in held-by when owned", () => {
    renderDisplayCard("shared-tv", sharedDisplay({ owned: true }), { scope: "shared" });

    expect(screen.getByText("shared-tv")).toBeInTheDocument();
    // Held-by column: shared + owned → "office-rule · office"
    expect(screen.getByText("office-rule · office")).toBeInTheDocument();
  });

  it("shared force blank has affects-all copy in confirm dialog", async () => {
    renderDisplayCard("shared-tv", sharedDisplay(), { scope: "shared" });

    // Button always says "Force blank"; the shared warning moved to the confirm dialog.
    const btn = screen.getByRole("button", { name: "Force blank" });
    expect(btn).toBeInTheDocument();
    // Clicking opens the confirm dialog with the shared-blank warning.
    fireEvent.click(btn);
    await waitFor(() => {
      expect(screen.getByRole("alertdialog", { name: /Force blank shared-tv/ })).toBeInTheDocument();
    });
  });

  it("renders basic private panel label and action buttons without KVM controls", () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["private-panel", { phase: "active", inhibited: false, paused: false, cmd_gen: 1, controllers: [] }]],
        pending_reload: null,
        kvm: { keymap: {}, switch_capable_displays: [], activity_following: false, push_capable_displays: [] },
      },
      displayConfigs: {
        "private-panel": { controllers: [], blank_mode: "power_off" } as DisplayConfig,
      },
      displayRules: { "private-panel": { rule: "office-rule", zone: "office" } },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);

    expect(screen.getByText("private-panel")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Force blank" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Force wake" })).toBeInTheDocument();
    // KVM switch/push buttons are absent when the display is not in either capability set.
    expect(screen.queryByRole("button", { name: "Pull" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Push" })).not.toBeInTheDocument();
  });

  it("switch-to-here (Pull) button calls postSwitch", async () => {
    renderDisplayCard("shared-tv", sharedDisplay());
    fireEvent.click(screen.getByRole("button", { name: "Pull" }));
    await waitFor(() => expect(mocks.postSwitch).toHaveBeenCalledWith("shared-tv"));
  });

  it("does not render push button when push not capable", () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["shared-tv", sharedDisplay()]],
        pending_reload: null,
        kvm: { keymap: {}, switch_capable_displays: ["shared-tv"], activity_following: false, push_capable_displays: [] },
      },
      displayConfigs: {
        "shared-tv": { controllers: [], blank_mode: "power_off", scope: "shared" } as DisplayConfig,
      },
      displayRules: { "shared-tv": { rule: "tv-rule", zone: "tv" } },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    expect(screen.queryByRole("button", { name: "Push" })).not.toBeInTheDocument();
  });

  it("send-to-peer (Push) button calls postPush, not postSwitch", async () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["shared-tv", sharedDisplay()]],
        pending_reload: null,
        kvm: { keymap: {}, switch_capable_displays: ["shared-tv"], activity_following: false, push_capable_displays: ["shared-tv"] },
      },
      displayConfigs: {
        "shared-tv": {
          controllers: [],
          blank_mode: "power_off",
          scope: "shared",
          shared_peer_input_write_code: 96,
        } as DisplayConfig,
      },
      displayRules: { "shared-tv": { rule: "tv-rule", zone: "tv" } },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    const btn = screen.getByRole("button", { name: "Push" });
    expect(btn).toBeInTheDocument();
    fireEvent.click(btn);
    await waitFor(() => expect(mocks.postPush).toHaveBeenCalledWith("shared-tv"));
    expect(mocks.postSwitch).not.toHaveBeenCalled();
  });

  it("surfaces a failed push as an error", async () => {
    mocks.postPush.mockRejectedValueOnce(new Error("write failed: DDC bus unreachable"));
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["shared-tv", sharedDisplay()]],
        pending_reload: null,
        kvm: { keymap: {}, switch_capable_displays: ["shared-tv"], activity_following: false, push_capable_displays: ["shared-tv"] },
      },
      displayConfigs: {
        "shared-tv": {
          controllers: [],
          blank_mode: "power_off",
          scope: "shared",
          shared_peer_input_write_code: 96,
        } as DisplayConfig,
      },
      displayRules: { "shared-tv": { rule: "tv-rule", zone: "tv" } },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    fireEvent.click(screen.getByRole("button", { name: "Push" }));
    expect(await screen.findByText("write failed: DDC bus unreachable")).toBeInTheDocument();
  });

  it("surfaces a failed switch as an error", async () => {
    mocks.postSwitch.mockRejectedValueOnce(new Error("write failed: DDC bus unreachable"));
    renderDisplayCard("shared-tv", sharedDisplay());
    fireEvent.click(screen.getByRole("button", { name: "Pull" }));
    expect(await screen.findByText("write failed: DDC bus unreachable")).toBeInTheDocument();
  });

  it("hides switch and push buttons when kvm is null", () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["shared-tv", sharedDisplay()]],
        pending_reload: null,
        // kvm absent — neither switch nor push is possible
      },
      displayConfigs: {
        "shared-tv": { controllers: [], blank_mode: "power_off", scope: "shared" } as DisplayConfig,
      },
      displayRules: { "shared-tv": { rule: "tv-rule", zone: "tv" } },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    expect(screen.queryByRole("button", { name: "Pull" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Push" })).not.toBeInTheDocument();
  });

  it("hides switch and push buttons when display not in capability sets", () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["shared-tv", sharedDisplay()]],
        pending_reload: null,
        kvm: { keymap: {}, switch_capable_displays: [], activity_following: false, push_capable_displays: [] },
      },
      displayConfigs: {
        "shared-tv": { controllers: [], blank_mode: "power_off", scope: "shared" } as DisplayConfig,
      },
      displayRules: { "shared-tv": { rule: "tv-rule", zone: "tv" } },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    expect(screen.queryByRole("button", { name: "Pull" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Push" })).not.toBeInTheDocument();
  });

  // BG-8: wear advisory join uses config_display_id when present,
  // falling back to display_name for backward compatibility.
  it("joins wear advisory via config_display_id when available", () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["panel-a", { phase: "active", inhibited: false, paused: false, cmd_gen: 1, controllers: [] }]],
        pending_reload: null,
      },
      displayConfigs: { "panel-a": { controllers: [], blank_mode: "power_off" } as DisplayConfig },
      displayRules: { "panel-a": { rule: "a-rule", zone: "a" } },
      wear: {
        displays: [{
          display: "ddc:AOC:1234",
          display_name: "old-name",
          config_display_id: "panel-a",
          panel_type: "woled",
          total_on_hours: 10,
          sample_count: 1,
          advisory: true,
          hours_since_long_dwell: 100,
        }],
      },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    // Advisory chip reaches the correct display tile via config_display_id.
    expect(screen.getByText("wear advisory")).toBeInTheDocument();
  });

  it("falls back to display_name when config_display_id is absent (pre-BG-8)", () => {
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["panel-b", { phase: "active", inhibited: false, paused: false, cmd_gen: 1, controllers: [] }]],
        pending_reload: null,
      },
      displayConfigs: { "panel-b": { controllers: [], blank_mode: "power_off" } as DisplayConfig },
      displayRules: { "panel-b": { rule: "b-rule", zone: "b" } },
      wear: {
        displays: [{
          display: "sanitized-key",
          display_name: "panel-b",
          panel_type: "unknown",
          total_on_hours: 5,
          sample_count: 1,
          advisory: true,
          hours_since_long_dwell: 50,
        }],
      },
    });
    render(<LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>);
    // Advisory chip reaches the correct display tile via display_name fallback.
    expect(screen.getByText("wear advisory")).toBeInTheDocument();
  });
});

  it("renders stage detail when a display is in the staged phase", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("lg-oled")).toBeInTheDocument();
    });

    // The chip should show "staged · render black" for the staged display.
    expect(screen.getByText("staged · render black")).toBeInTheDocument();
  });

  it("does not render stage detail for non-staged displays", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // The active display shows its phase label as "active" (unchanged).
    expect(screen.getByText("active")).toBeInTheDocument();
    // The blanked display shows "blanked".
    expect(screen.getByText("blanked")).toBeInTheDocument();

    // The stage-detail label only appears ONCE — for the staged display.
    const stageLabels = screen.getAllByText(/render black/);
    expect(stageLabels).toHaveLength(1);
  });

function DisplaysDetailHarness() {
  const [selectedDisplay, selectDisplay] = useState<string | null>(null);
  const state = liveStateFixture({
    snapshot: {
      sensors: [],
      zones: [],
      displays: [["main", {
        phase: "active",
        inhibited: false,
        paused: false,
        cmd_gen: 1,
        controllers: [{ name: "ddcci", role: "primary", healthy: true }],
      }]],
      pending_reload: null,
    },
    displayConfigs: {
      main: { controllers: ["ddcci"], blank_mode: "power_off" } as DisplayConfig,
    },
    displayRules: { main: { rule: "office-rule", zone: "office" } },
    wearDetails: {
      main: {
        display: "panel-main",
        display_name: "main",
        panel_type: "woled",
        total_on_hours: 4,
        sample_count: 8,
        advisory: false,
        hours_since_long_dwell: 1,
        grid_rows: 1,
        grid_cols: 2,
        cells: [1, 2],
        heat: [0, 1],
      },
    },
    selectedDisplay,
    selectDisplay,
  });
  return <LiveStateContext.Provider value={state}><Displays /></LiveStateContext.Provider>;
}

it("switches between the display list and selected detail in one view", () => {
  render(<DisplaysDetailHarness />);
  fireEvent.click(screen.getByRole("button", { name: "Detail →" }));
  expect(screen.getByRole("grid", { name: "main panel wear heat map" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "← Displays" }));
  expect(screen.getByRole("button", { name: "Detail →" })).toBeInTheDocument();
});

/**
 * Displays component test — renders per-display cards with actions,
 * plus the list/detail-mode switch.
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
  return {
    mocks: { postBlank, postWake, postPause, postResume },
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

function renderDisplayCard(id: string, display: DisplaySnapshot) {
  const state = liveStateFixture({
    snapshot: {
      sensors: [],
      zones: [],
      displays: [[id, display]],
      pending_reload: null,
    },
    displayConfigs: {
      [id]: { controllers: [], blank_mode: "power_off" } as DisplayConfig,
    },
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
  it("renders display cards with IDs and phases", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("samsung-tv")).toBeInTheDocument();
    expect(screen.getByText("active")).toBeInTheDocument();
    expect(screen.getByText("blanked")).toBeInTheDocument();
  });

  it("renders paused and inhibited chips", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("paused")).toBeInTheDocument();
    });
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

  it("renders metric fields", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // "Blank mode" etc. appear in every display card — use getAllByText
    expect(screen.getAllByText("Blank mode").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Driven by zone").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Rule").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Cmd gen").length).toBeGreaterThanOrEqual(1);
  });

  it("renders zone and rule from display_rules reverse lookup", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // Both displays map to the "office" zone in the fixture
    expect(screen.getAllByText("office").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("office-rule").length).toBeGreaterThanOrEqual(1);
  });

  it("calls postBlank guarded by confirmation, and postPause guarded by confirmation, with correct ids", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });
    expect(screen.getByText("samsung-tv")).toBeInTheDocument();

    // First card (aoc-main): not paused → "Pause rule", "Force blank", "Force wake".
    // Force blank/Pause each share one confirm dialog, so each trigger click
    // hides every card's action row — the dialog's own button is the sole
    // remaining element with that accessible name.
    fireEvent.click(screen.getAllByText("Force blank")[0]);
    expect(screen.getByRole("alertdialog", { name: "Force blank aoc-main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Force blank" }));
    await waitFor(() => expect(mocks.postBlank).toHaveBeenCalledWith("aoc-main"));

    // Pause rule on first display (aoc-main → rule "office-rule")
    fireEvent.click(screen.getAllByText("Pause rule")[0]);
    expect(screen.getByRole("alertdialog", { name: "Pause office-rule?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Pause rule" }));
    await waitFor(() => expect(mocks.postPause).toHaveBeenCalledWith({ rule: "office-rule" }));
  });

  // P1-F: Force wake and Resume are non-destructive (wake just lights the
  // panel) — the proto's friction model leaves them un-gated. No confirm
  // dialog should appear; the click posts immediately.
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

    // `confirm()`'s promise resolves synchronously inside the Cancel
    // button's onClick (useConfirmDialog's `finish`), so the `.then`
    // continuation that would call postBlank is only scheduled as a
    // microtask — it hasn't run yet immediately after `fireEvent.click`
    // returns. Flush microtasks before asserting "not called" so a
    // mutant that ignores `accepted` doesn't pass by accident.
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

  it("shared card uses panel state instead of local phase", () => {
    renderDisplayCard("shared-tv", sharedDisplay());

    expect(screen.getByText("○ OFF")).toBeInTheDocument();
    expect(screen.getByText("owner")).toBeInTheDocument();
  });

  it("deferred shared card keeps force wake enabled", () => {
    renderDisplayCard("shared-tv", sharedDisplay({ owned: false }));

    expect(screen.getByText("deferred")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Force wake" })).toBeEnabled();
  });

  it("shared force blank has affects-all copy", () => {
    renderDisplayCard("shared-tv", sharedDisplay());

    expect(screen.getByRole("button", {
      name: "Blank shared panel — affects all connected machines",
    })).toBeInTheDocument();
  });

  it("private card copy unchanged", () => {
    renderDisplayCard("private-panel", {
      phase: "active",
      inhibited: false,
      paused: false,
      cmd_gen: 1,
      controllers: [],
    });

    expect(screen.getByText("● ON")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Force blank" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Force wake" })).toBeInTheDocument();
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
  fireEvent.click(screen.getByRole("button", { name: "Open main detail" }));
  expect(screen.getByRole("grid", { name: "main panel wear heat map" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "← Displays" }));
  expect(screen.getByRole("button", { name: "Open main detail" })).toBeInTheDocument();
});

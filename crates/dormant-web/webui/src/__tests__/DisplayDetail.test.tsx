import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import DisplayDetail from "../app/views/DisplayDetail";
import { normalizeWearGrid } from "../app/components/WearHeatMap";
import type { DisplayConfig, WearDetail } from "../api/types";

const api = vi.hoisted(() => ({
  postBlank: vi.fn().mockResolvedValue(undefined),
  postWake: vi.fn().mockResolvedValue(undefined),
  postPause: vi.fn().mockResolvedValue(undefined),
  postResume: vi.fn().mockResolvedValue(undefined),
  postExercise: vi.fn().mockResolvedValue({
    display: "main",
    pre_phase: "active",
    paused_rules: [],
    steps: [],
  }),
  // Adaptation: explicit-field mock class — see ExerciseRunner.test.tsx's
  // comment (tsconfig `erasableSyntaxOnly` rejects parameter properties).
  ApiError: class ApiError extends Error {
    status: number;
    body: unknown;
    constructor(status: number, body: unknown) {
      super(`API ${status}`);
      this.status = status;
      this.body = body;
    }
  },
}));

vi.mock("../api/client", () => api);
vi.mock("../app/nav", () => ({
  useNavigate: () => vi.fn(),
}));
vi.mock("../app/hooks/useLiveState", () => ({
  useLiveState: () => ({
    operations: {
      exercise_in_flight: [],
      emergency_wake_in_flight: false,
    },
    operationsRequestId: 1,
    refreshOperations: vi.fn().mockResolvedValue({
      exercise_in_flight: [],
      emergency_wake_in_flight: false,
    }),
  }),
}));

const snapshot = {
  phase: "active",
  inhibited: false,
  paused: false,
  cmd_gen: 41,
  controllers: [
    { name: "ddcci", role: "primary" as const, healthy: true },
    { name: "kwin-dpms", role: "fallback" as const, healthy: false, detail: "timeout" },
  ],
};

const wear = {
  display: "panel-main",
  display_name: "main",
  panel_type: "woled" as const,
  total_on_hours: 321.25,
  sample_count: 444,
  advisory: true,
  hours_since_long_dwell: 120,
  grid_rows: 2,
  grid_cols: 2,
  cells: [1, 2, 0.5, 2.5],
  heat: [0.1, 0.4, 0.4, 0.9],
};

afterEach(() => { cleanup(); vi.clearAllMocks(); });

describe("DisplayDetail", () => {
  it("renders the heat map, summaries, controller chain, and guarded controls", async () => {
    render(
      <DisplayDetail
        id="main"
        snapshot={snapshot}
        config={{ controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );

    expect(screen.getByRole("grid", { name: "main panel wear heat map" })).toBeInTheDocument();
    expect(screen.getAllByRole("gridcell")).toHaveLength(4);

    // Heat card eyebrow, caption, panel-type chip, legend, honesty note (F2).
    expect(screen.getByText("PANEL WEAR HEAT MAP", { exact: false, selector: ".display-detail__eyebrow" })).toBeInTheDocument();
    expect(screen.getByText("2×2 grid · per-cell brightness-weighted on-hours")).toBeInTheDocument();
    expect(screen.getAllByText("WOLED").length).toBeGreaterThan(0);
    expect(screen.getByText("low")).toBeInTheDocument();
    expect(screen.getByText("high")).toBeInTheDocument();
    expect(screen.getByText(/v1 attribution is panel-wide and advisory/)).toBeInTheDocument();

    // Exposure summary tiles (F3). W3-3: when not seeded, "Total on-hours"
    // replaces the seeded/measured split.
    expect(screen.getByText("321.3h")).toBeInTheDocument();
    expect(screen.getByText("Total on-hours")).toBeInTheDocument();
    expect(screen.getByText("444")).toBeInTheDocument();
    expect(screen.getByText("5d")).toBeInTheDocument();
    // W3-3: last-sample relative timestamp and panel type are new tiles.
    expect(screen.getByText("Last sample")).toBeInTheDocument();
    expect(screen.getByText("Panel type")).toBeInTheDocument();
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.getByText("45%")).toBeInTheDocument();

    expect(screen.getByText("kwin-dpms")).toBeInTheDocument();
    expect(screen.getByText("timeout")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /emergency wake/i })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeInTheDocument();

    // F4 — Cmd gen fact value is the bare number, not "Command generation N".
    expect(screen.getByText("41", { selector: ".display-detail__fact-value" })).toBeInTheDocument();
    expect(screen.queryByText(/Command generation/)).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Force blank" }));
    expect(screen.getByRole("alertdialog", { name: "Force blank main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Force blank" }));
    await waitFor(() => expect(api.postBlank).toHaveBeenCalledWith("main"));
  });

  // P1-F: Force wake is non-destructive — un-gated, no confirm dialog.
  // Mirrors Displays.test.tsx's "posts wake/resume immediately with no
  // confirm dialog" coverage at the detail-view level.
  it("posts wake immediately with no confirm dialog", async () => {
    render(
      <DisplayDetail
        id="main"
        snapshot={snapshot}
        config={{ controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Force wake" }));
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    await waitFor(() => expect(api.postWake).toHaveBeenCalledWith("main"));
  });

  // P1-F: Resume is non-destructive — un-gated, no confirm dialog. Only
  // renders when the display is paused (the unpaused branch shows Pause
  // rule instead, already exercised by the primary render test above).
  it("posts resume immediately with no confirm dialog when the display is paused", async () => {
    render(
      <DisplayDetail
        id="main"
        snapshot={{ ...snapshot, paused: true }}
        config={{ controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Resume rule" }));
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    await waitFor(() => expect(api.postResume).toHaveBeenCalledWith({ rule: "office_blank" }));
  });

  it("does not post when the force blank confirmation is cancelled", async () => {
    render(
      <DisplayDetail
        id="main"
        snapshot={snapshot}
        config={{ controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );

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

    fireEvent.click(screen.getByRole("button", { name: "Force blank" }));
    expect(screen.getByRole("alertdialog", { name: "Force blank main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flush();
    expect(api.postBlank).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  });

  it("renders an honest empty state when no heat grid exists", () => {
    render(
      <DisplayDetail
        id="main"
        snapshot={snapshot}
        config={{ controllers: ["ddcci"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={undefined}
        wearError={null}
        onBack={vi.fn()}
      />,
    );
    expect(screen.getByText("No spatial wear samples for this display yet.")).toBeInTheDocument();
  });

  it("normalizes empty, short, long, non-finite, and zero-dimension arrays", () => {
    const base = {
      ...wear,
      grid_rows: 2,
      grid_cols: 2,
    } satisfies WearDetail;

    expect(normalizeWearGrid({ ...base, cells: [], heat: [] })).toMatchObject({
      weightedHours: [0, 0, 0, 0],
      heat: [0, 0, 0, 0],
      hasGridSamples: false,
      hasHeatSamples: false,
      averageHeat: null,
      uniformity: null,
    });
    expect(normalizeWearGrid({ ...base, cells: [2], heat: [0.5] })).toMatchObject({
      weightedHours: [2, 0, 0, 0],
      heat: [0.5, 0, 0, 0],
      averageHeat: 0.125,
    });
    expect(normalizeWearGrid({
      ...base,
      cells: [1, 2, 3, 4, 99],
      heat: [0, 0.25, 0.5, 1, 0.75],
    })).toMatchObject({
      weightedHours: [1, 2, 3, 4],
      heat: [0, 0.25, 0.5, 1],
    });
    expect(normalizeWearGrid({
      ...base,
      cells: [Number.NaN, Number.POSITIVE_INFINITY],
      heat: [Number.NaN, Number.NEGATIVE_INFINITY],
    })).toMatchObject({
      weightedHours: [0, 0, 0, 0],
      heat: [0, 0, 0, 0],
      hasHeatSamples: false,
      averageHeat: null,
    });
    expect(normalizeWearGrid({ ...base, grid_rows: 0, grid_cols: 2 })).toMatchObject({
      rows: 0,
      cols: 2,
      weightedHours: [],
      heat: [],
      hasGridSamples: false,
      hasHeatSamples: false,
    });
  });

  // W3-2 acceptance: Sharing section absent for private displays.
  it("does not render Sharing section for private displays", () => {
    render(
      <DisplayDetail
        id="main"
        snapshot={{ ...snapshot, scope: "private" }}
        config={{ controllers: ["ddcci"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );
    expect(screen.queryByRole("heading", { name: "Sharing" })).not.toBeInTheDocument();
  });

  // W3-2 acceptance: Sharing section renders for shared displays.
  it("renders Sharing section with OwnershipPair for shared displays", () => {
    render(
      <DisplayDetail
        id="shared-panel"
        snapshot={{
          phase: "active",
          inhibited: false,
          paused: false,
          cmd_gen: 1,
          controllers: [],
          scope: "shared",
          owned: true,
          observed_input_code: 96,
          panel_state: { power: "on" },
        }}
        config={{
          controllers: ["ddcci"],
          blank_mode: "power_off",
          scope: "shared",
          shared_input_code: 96,
          shared_input_write_code: 96,
          shared_peer_input_code: 0,
        } as DisplayConfig}
        rule={{ rule: "shared-rule", zone: "shared-zone" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );
    expect(screen.getByRole("heading", { name: "Sharing" })).toBeInTheDocument();
    // OwnershipPair renders the compact ownership state.
    expect(screen.getByText(/holds the panel/)).toBeInTheDocument();
    // The "open Switching →" link is present.
    expect(screen.getByText(/open Switching/)).toBeInTheDocument();
  });

  // W3-2 acceptance: #/displays/{id}#wear scrolls to and highlights the section.
  it("scrolls to and highlights the target anchor section", () => {
    const scrollIntoView = vi.fn();
    Element.prototype.scrollIntoView = scrollIntoView;
    // Simulate a deep-link hash fragment.
    vi.stubGlobal("location", { hash: "#/displays/main#wear" });

    render(
      <DisplayDetail
        id="main"
        snapshot={snapshot}
        config={{ controllers: ["ddcci"], blank_mode: "power_off" } as DisplayConfig}
        rule={{ rule: "office_blank", zone: "office" }}
        wear={wear}
        wearError={null}
        onBack={vi.fn()}
      />,
    );
    // The useEffect defers 150ms; wait for it.
    return new Promise<void>((resolve) => {
      setTimeout(() => {
        expect(scrollIntoView).toHaveBeenCalled();
        vi.unstubAllGlobals();
        resolve();
      }, 200);
    });
  });
});
